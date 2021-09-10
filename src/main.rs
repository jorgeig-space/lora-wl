#![no_std]
#![no_main]

mod lorawan;
mod lorawan_crypto;
mod rfswitch;

use defmt_rtt as _;
use stm32wl_hal as hal;
use panic_probe as _;

use core::convert::TryFrom;

use hal::{
    cortex_m::prelude::_embedded_hal_timer_CountDown,
    gpio::{PortA, PortC, RfNssDbg, SgMisoDbg, SgMosiDbg, SgSckDbg}, 
    lptim::{LpTim, LpTim1}, 
    pac as pac, 
    rng::Rng,
    spi::{SgMiso, SgMosi}, subghz::*};

use lorawan::{
    Event as LoraEvent,
    LorawanRadio
};
use lorawan_crypto::LorawanCrypto as Crypto;
use lorawan_device::{
    Device as LorawanDevice, 
    Error as LorawanError,
    Event as LorawanEvent, 
    radio,
    region::Configuration, Region,
    Response as LorawanResponse,
};

use rfswitch::*;

/// Get a random u32 from the RNG peripheral
/// WARNING: This function assumes that the RNG has been initialized and its clock is enabled
/// Enable the RNG upon wakeup to use it for the whole session
fn get_random_u32() -> u32 {
    let mut rng = unsafe { Rng::steal() };
    rng.try_u32().unwrap_or(0xFAFAFAFA) // Obviously don't ever do this in production
}

#[rtic::app(device = crate::pac, peripherals = true)]
const APP: () = {
    struct Resources<'a> {
        #[init([0;256])]
        buffer_tx: [u8; 256],
        #[init([0;256])]
        buffer_rx: [u8; 256],
        lorawan: Option<LorawanDevice<'static, LorawanRadio, Crypto>>,
        lptim: LpTim1,
        rcc: pac::RCC,
    }

    #[init(spawn = [lorawan_event], resources=[buffer_tx])]
    fn init(ctx: init::Context) -> init::LateResources {
        let mut dp: pac::Peripherals = ctx.device;

        defmt::info!("Init start");

        let gpioa: PortA = PortA::split(dp.GPIOA, &mut dp.RCC);
        let gpioc: PortC = PortC::split(dp.GPIOC, &mut dp.RCC);
        let mut rfs: RfSwitch = RfSwitch::new(gpioc.c3, gpioc.c4, gpioc.c5);
        rfs.set_rx();

        let sg: SubGhz<SgMiso, SgMosi> = SubGhz::new(dp.SPI3, &mut dp.RCC);
        // For future use
        let _: RfNssDbg = RfNssDbg::new(gpioa.a4);
        let _: SgSckDbg = SgSckDbg::new(gpioa.a5);
        let _: SgMisoDbg = SgMisoDbg::new(gpioa.a6);
        let _: SgMosiDbg = SgMosiDbg::new(gpioa.a7);

        let lora_sg = lorawan::LorawanRadio::new(sg, rfs, false);

        dp.RCC.csr.modify(|_,w| w
            .lsion().on());
        while dp.RCC.csr.read().lsirdy().is_not_ready() {}
        // In this case we use LSI @ 32 KHz with Div32 Prescaler
        // That means 1 ms is 1 cycles
        let mut lptim: LpTim1 = LpTim1::new(dp.LPTIM1, hal::lptim::Clk::Lsi, hal::lptim::Prescaler::Div32, &mut dp.RCC);
        lptim.set_ier(hal::lptim::irq::CMPM);

        let rng = Rng::new(dp.RNG, hal::rng::Clk::MSI, &mut dp.RCC);

        ctx.spawn
            .lorawan_event(LorawanEvent::NewSessionRequest)
            .unwrap();

        defmt::info!("Init complete");
        init::LateResources {
            lorawan: Some(LorawanDevice::new(
                Configuration::new(Region::EU433),
                lora_sg,
                [0xE4, 0xE3, 0xE2, 0xE1, 0xF5, 0xF4, 0xF3, 0xFd], 
                [0x04, 0x03, 0x02, 0x01, 0x04, 0x03, 0x02, 0x01], 
                [0xA9, 0xA8, 0xA7, 0xA6, 0xA5, 0xA4, 0xA3, 0xA2,
                0xA9, 0xA8, 0xA7, 0xA6, 0xA5, 0xA4, 0xA3, 0xA2],
                get_random_u32,
                ctx.resources.buffer_tx,
            )),
            lptim,
            rcc: dp.RCC
        }
    }

    #[task(priority = 2, resources = [lorawan, rcc], spawn = [lorawan_response])]
    fn lorawan_event(ctx: lorawan_event::Context, event: LorawanEvent<'static, LorawanRadio>) {

        // Enable rng clock so lorawan can use the RNG peripheral
        Rng::enable_clock(ctx.resources.rcc);

        // The LoraWAN stack is a giant state machine which needs to mutate internally
        // We let that happen within RTIC's framework for shared statics
        // by using an Option cell that we can take() from
        if let Some(lorawan) = ctx.resources.lorawan.take() {
            // debug statements for the event
            match &event {
                LorawanEvent::NewSessionRequest => {
                    defmt::info!("New Session Request");
                }
                LorawanEvent::RadioEvent(e) => match e {
                    radio::Event::TxRequest(_, _) => defmt::info!("TxRequest in task `lorawan_event`"),
                    radio::Event::RxRequest(_) => defmt::info!("RxRequest in task `lorawan_event`"),
                    radio::Event::CancelRx => defmt::info!("CancelRx in task `lorawan_event`"),
                    radio::Event::PhyEvent(phy) => {
                        let event = phy as &lorawan::Event;
                        match event {
                            lorawan::Event::Irq(status, irq_status) => {
                                defmt::info!("Radio Rx/Tx Interrupt: {}", irq_status);
                            }
                        }
                    }
                },
                LorawanEvent::TimeoutFired => {
                    defmt::info!("TimeoutFired");
                },
                LorawanEvent::SendDataRequest(_e) => {
                    defmt::info!("SendData");
                }
            }
            let (new_state, response) = lorawan.handle_event(event);
            ctx.spawn.lorawan_response(response).unwrap();

            // placing back into the Option cell after taking is critical
            ctx.resources.lorawan.replace(new_state);
        }
    }

    #[task(priority = 2, resources = [lorawan], spawn = [lorawan_event, set_timer])]
    fn lorawan_response(
        mut ctx: lorawan_response::Context,
        response: Result<LorawanResponse, LorawanError<LorawanRadio>>,
    ) {
        match response {
            Ok(response) => match response {
                LorawanResponse::TimeoutRequest(ms) => {
                    defmt::info!("TimeoutRequest: {}", ms);
                    ctx.spawn.set_timer(u16::try_from(ms).unwrap());
                }
                LorawanResponse::JoinSuccess => {
                    if let Some(lorawan) = ctx.resources.lorawan.take() {
                        defmt::info!("Join Success");
                        *ctx.resources.lorawan = Some(lorawan);
                    }
                }
                LorawanResponse::ReadyToSend => {
                    defmt::info!("RxWindow expired but no ACK expected. Ready to Send");
                }
                LorawanResponse::DownlinkReceived(fcnt_down) => {
                    defmt::info!("DownlinkReceived: fcnt_down = {}", fcnt_down);
                    if let Some(mut lorawan) = ctx.resources.lorawan.take() {
                        if let Some(downlink) = lorawan.take_data_downlink() {
                            let fhdr = downlink.fhdr();
                            let fopts = fhdr.fopts();
                            use lorawan_encoding::parser::{DataHeader, FRMPayload};

                            if let Ok(FRMPayload::Data(data)) = downlink.frm_payload() {
                                //defmt::info!("Downlink received (FCntDown={} FRM: {})", fcnt_down, data);
                            } else {
                                //defmt::info!("Downlink received (FcntDown={})", fcnt_down);
                            }

                            let mut mac_commands_len = 0;
                            for mac_command in fopts {
                                if mac_commands_len == 0 {
                                    defmt::info!("FOpts: ");
                                }
                                // TODO implement fmt for lorawan-encoding structs
                                defmt::info!("MAC Command");
                                mac_commands_len += 1;
                            }
                        }

                        // placing back into the Option cell after taking is critical
                        *ctx.resources.lorawan = Some(lorawan);
                    }
                }
                LorawanResponse::NoAck => {
                    defmt::info!("RxWindow expired, expected ACK to confirmed uplink not received");
                }
                LorawanResponse::NoJoinAccept => {
                    defmt::info!("No Join Accept Received");
                    ctx.spawn
                        .lorawan_event(LorawanEvent::NewSessionRequest)
                        .unwrap();
                }
                LorawanResponse::SessionExpired => {
                    defmt::info!("SessionExpired. Created new Session");
                    ctx.spawn
                        .lorawan_event(LorawanEvent::NewSessionRequest)
                        .unwrap();
                }
                LorawanResponse::NoUpdate => (),
                LorawanResponse::UplinkSending(fcnt_up) => {
                    defmt::info!("Uplink with FCnt {}", fcnt_up);
                }
                LorawanResponse::JoinRequestSending => {
                    defmt::info!("Join Request Sending");
                }
            },
            Err(err) => match err {
                LorawanError::Radio(_) => defmt::info!("Error Radio "),
                LorawanError::Session(e) => defmt::info!("Error Session"),
                LorawanError::NoSession(_) => defmt::info!("Error NoSession"),
            },
        }
    }

    #[task(resources=[lptim], priority = 3)]
    fn set_timer(mut ctx: set_timer::Context, ms: u16) {
            ctx.resources.lptim.lock(|lptim| {
            
            if hal::lptim::LpTim1::cnt() != 0 {
                defmt::error!("Asking for Timer but it is already running, count: {}, asking: {}", hal::lptim::LpTim1::cnt(), ms);
            } else {
                // The clock is set at 32 Khz with Div32 prescaler
                // and the LPtim counts clock events, so 1 ms = 1 cycle
                lptim.start(ms);
            }
        });
    }

    #[task(binds=LPTIM1, priority = 4, resources=[lptim], spawn=[lorawan_event])]
    fn timer_irq(ctx: timer_irq::Context) {
        //defmt::debug!("LPTim interrupt triggered, ISR: {}", hal::lptim::LpTim1::isr());
        unsafe { ctx.resources.lptim.set_icr(hal::lptim::irq::CMPM); }
        ctx.spawn.lorawan_event(LorawanEvent::TimeoutFired).unwrap();
    }

    #[task(binds=RADIO_IRQ_BUSY, priority = 4, resources=[], spawn=[lorawan_event])]
    fn radio_irq(ctx: radio_irq::Context) {
        let mut subghz = unsafe { hal::subghz::SubGhz::steal() };
        let (status, irq_status) = subghz.irq_status().unwrap();
        subghz.clear_irq_status(irq_status);
        //defmt::debug!("Radio IRQ: {}", irq_status);
        ctx.spawn.lorawan_event(LorawanEvent::RadioEvent(radio::Event::PhyEvent(LoraEvent::Irq(status, irq_status))));
    }

    extern "C" {
        fn TIM16();
        fn TIM17();
        fn USART1();
    }
};