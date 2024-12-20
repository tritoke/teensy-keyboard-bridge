//! Demonstrates a USB keypress using RTIC.
//!
//! Flash your board with this example. Your device will occasionally
//! send some kind of keypress to your host.

#![no_std]
#![no_main]

use teensy4_panic as _;

#[rtic::app(device = teensy4_bsp, peripherals = false)]
mod app {
    use circular_buffer::CircularBuffer;
    use rtic_monotonics::rtic_time::embedded_hal::digital::OutputPin;
    use teensy4_bsp::{self as bsp, board};

    use bsp::hal::{
        lpuart,
        usbd::{gpt, BusAdapter, EndpointMemory, EndpointState, Speed},
    };

    use usb_device::{
        bus::UsbBusAllocator,
        device::{UsbDevice, UsbDeviceBuilder, UsbDeviceState, UsbVidPid},
    };
    use usbd_hid::{
        descriptor::{KeyboardReport, KeyboardUsage, SerializedDescriptor as _},
        hid_class::HIDClass,
    };

    /// Change me if you want to play with a full-speed USB device.
    const SPEED: Speed = Speed::High;
    /// Matches whatever is in imxrt-log.
    const VID_PID: UsbVidPid = UsbVidPid(0x5824, 0x27dd);
    const PRODUCT: &str = "teensy-keyboard-bridge";
    /// How frequently should we poll the logger?
    const LPUART_POLL_INTERVAL_MS: u32 = board::PERCLK_FREQUENCY / 1_000 * 100;
    /// The USB GPT timer we use to (infrequently) send mouse updates.
    const GPT_INSTANCE: gpt::Instance = gpt::Instance::Gpt0;
    /// How frequently should we push keyboard updates to the host?
    const KEYBOARD_UPDATE_INTERVAL_MS: u32 = 20;

    /// This allocation is shared across all USB endpoints. It needs to be large
    /// enough to hold the maximum packet size for *all* endpoints. If you start
    /// noticing panics, check to make sure that this is large enough for all endpoints.
    static EP_MEMORY: EndpointMemory<1024> = EndpointMemory::new();
    /// This manages the endpoints. It's large enough to hold the maximum number
    /// of endpoints; we're not using all the endpoints in this example.
    static EP_STATE: EndpointState = EndpointState::max_endpoints();

    type Bus = BusAdapter;

    #[local]
    struct Local {
        class: HIDClass<'static, Bus>,
        device: UsbDevice<'static, Bus>,
        led: board::Led,
        lpuart2: board::Lpuart2,
    }

    #[shared]
    struct Shared {
        keys_to_press: circular_buffer::CircularBuffer<20, KeyboardReport>,
    }

    #[init(local = [bus: Option<UsbBusAllocator<Bus>> = None])]
    fn init(ctx: init::Context) -> (Shared, Local) {
        let board::Resources {
            pit: (mut timer, _, _, _),
            usb: usbd,
            pins,
            lpuart2,
            mut gpio2,
            ..
        } = board::t41(board::instances());
        let led = board::led(&mut gpio2, pins.p13);

        timer.set_load_timer_value(LPUART_POLL_INTERVAL_MS);
        timer.set_interrupt_enable(true);
        timer.enable();

        let mut lpuart2: board::Lpuart2 = board::lpuart(lpuart2, pins.p14, pins.p15, 115200);
        lpuart2.disable(|lpuart2| {
            lpuart2.disable_fifo(lpuart::Direction::Tx);
            lpuart2.disable_fifo(lpuart::Direction::Rx);
            lpuart2.set_interrupts(lpuart::Interrupts::RECEIVE_FULL);
            lpuart2.set_parity(None);
        });

        let bus = BusAdapter::with_speed(usbd, &EP_MEMORY, &EP_STATE, SPEED);
        bus.set_interrupts(true);
        bus.gpt_mut(GPT_INSTANCE, |gpt| {
            gpt.stop();
            gpt.clear_elapsed();
            gpt.set_interrupt_enabled(true);
            gpt.set_mode(gpt::Mode::Repeat);
            gpt.set_load(KEYBOARD_UPDATE_INTERVAL_MS * 1000);
            gpt.reset();
            gpt.run();
        });

        let bus = ctx.local.bus.insert(UsbBusAllocator::new(bus));
        // Note that "4" correlates to a 1ms polling interval. Since this is a high speed
        // device, bInterval is computed differently.
        let class = HIDClass::new(bus, KeyboardReport::desc(), 4);
        let device = UsbDeviceBuilder::new(bus, VID_PID)
            .strings(&[usb_device::device::StringDescriptors::default().product(PRODUCT)])
            .unwrap()
            .device_class(usbd_serial::USB_CLASS_CDC)
            .max_packet_size_0(64)
            .unwrap()
            .build();

        (
            Shared {
                keys_to_press: CircularBuffer::new(),
            },
            Local {
                class,
                device,
                led,
                lpuart2,
            },
        )
    }

    #[task(binds = USB_OTG1, local = [device, class, led, configured: bool = false], shared = [keys_to_press], priority = 2)]
    fn usb1(ctx: usb1::Context) {
        let usb1::LocalResources {
            class,
            device,
            led,
            configured,
            ..
        } = ctx.local;
        let mut keys_to_press = ctx.shared.keys_to_press;

        device.poll(&mut [class]);

        if device.state() == UsbDeviceState::Configured {
            if !*configured {
                device.bus().configure();
            }
            *configured = true;
        } else {
            *configured = false;
        }

        if !*configured {
            return;
        }

        let elapsed = device.bus().gpt_mut(GPT_INSTANCE, |gpt| {
            let elapsed = gpt.is_elapsed();
            while gpt.is_elapsed() {
                gpt.clear_elapsed();
            }
            elapsed
        });

        if !elapsed {
            return;
        }

        if let Some(key) = keys_to_press.lock(|keys| keys.pop_front()) {
            class.push_input(&key).ok();
            led.set_high().ok();
        } else {
            class.push_input(&KeyboardReport::default()).ok();
            led.set_low().ok();
        }
    }

    #[task(binds = LPUART2, local = [lpuart2, state: StateMachine = StateMachine::Start], shared = [keys_to_press], priority = 3)]
    fn lpuart2_interrupt(ctx: lpuart2_interrupt::Context) {
        use lpuart::Status;
        let lpuart2 = ctx.local.lpuart2;
        let state = ctx.local.state;
        let mut keys_to_press = ctx.shared.keys_to_press;

        let status = lpuart2.status();
        lpuart2.clear_status(Status::W1C);

        if status.contains(Status::RECEIVE_FULL) {
            loop {
                let data = lpuart2.read_data();
                if data.flags().contains(lpuart::ReadFlags::RXEMPT) {
                    break;
                }

                if let Some(key) = state.step(data.into()) {
                    keys_to_press.lock(|keys| {
                        keys.push_back(key);
                    });
                }
            }
        }
    }

    // State machine for parsing some ANSI escape sequences
    #[derive(Default, PartialEq, Eq)]
    enum StateMachine {
        /// We've seen nothing
        #[default]
        Start,

        /// We've just escape - 0x1B
        Escape,

        /// We've seen 0x1B then 0x5B
        Bracket,
    }

    impl StateMachine {
        fn step(&mut self, data: u8) -> Option<KeyboardReport> {
            match self {
                StateMachine::Start if data == 0x1B => *self = StateMachine::Escape,
                StateMachine::Start => return translate_char(data),
                StateMachine::Escape if data == b'[' => *self = StateMachine::Bracket,
                StateMachine::Bracket => {
                    *self = StateMachine::Start;
                    return match data {
                        b'A' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardUpArrow),
                        b'B' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardDownArrow),
                        b'C' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardRightArrow),
                        b'D' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardLeftArrow),
                        _ => None,
                    };
                }

                _ => *self = StateMachine::Start,
            }

            None
        }
    }

    // no modifier
    const MOD_NORM: u8 = 0;

    // "alt" modifier - left shift
    const MOD_ALT: u8 = 2;

    fn simple_kr(modifier: u8, keycode: impl Into<KeyboardUsage>) -> Option<KeyboardReport> {
        Some(KeyboardReport {
            modifier,
            reserved: 0,
            leds: 0,
            keycodes: [keycode.into() as u8, 0, 0, 0, 0, 0],
        })
    }

    fn translate_char(ch: u8) -> Option<KeyboardReport> {
        // this is a slightly dumb mapping from the UK keymap back to keyboard codes lol
        match ch {
            b'a'..=b'z' => {
                let base = KeyboardUsage::KeyboardAa as u8;
                let code = base + (ch - b'a');
                simple_kr(MOD_NORM, code)
            }
            b'A'..=b'Z' => {
                let base = KeyboardUsage::KeyboardAa as u8;
                let code = base + (ch - b'A');
                simple_kr(MOD_ALT, code)
            }
            b'1'..=b'9' => {
                let base = KeyboardUsage::Keyboard1Exclamation as u8;
                let code = base + (ch - b'1');
                simple_kr(MOD_NORM, code)
            }
            b'0' => simple_kr(MOD_NORM, KeyboardUsage::Keyboard0CloseParens),
            b'!' => simple_kr(MOD_ALT, KeyboardUsage::Keyboard1Exclamation),
            b'"' => simple_kr(MOD_ALT, KeyboardUsage::Keyboard2At),
            b'$' => simple_kr(MOD_ALT, KeyboardUsage::Keyboard4Dollar),
            b'%' => simple_kr(MOD_ALT, KeyboardUsage::Keyboard5Percent),
            b'^' => simple_kr(MOD_ALT, KeyboardUsage::Keyboard6Caret),
            b'&' => simple_kr(MOD_ALT, KeyboardUsage::Keyboard7Ampersand),
            b'*' => simple_kr(MOD_ALT, KeyboardUsage::Keyboard8Asterisk),
            b'(' => simple_kr(MOD_ALT, KeyboardUsage::Keyboard9OpenParens),
            b')' => simple_kr(MOD_ALT, KeyboardUsage::Keyboard0CloseParens),
            b' ' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardSpacebar),
            b'-' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardDashUnderscore),
            b'_' => simple_kr(MOD_ALT, KeyboardUsage::KeyboardDashUnderscore),
            b'=' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardEqualPlus),
            b'+' => simple_kr(MOD_ALT, KeyboardUsage::KeyboardEqualPlus),
            b'[' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardOpenBracketBrace),
            b'{' => simple_kr(MOD_ALT, KeyboardUsage::KeyboardOpenBracketBrace),
            b']' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardCloseBracketBrace),
            b'}' => simple_kr(MOD_ALT, KeyboardUsage::KeyboardCloseBracketBrace),
            b'\'' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardSingleDoubleQuote),
            b'\\' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardBackslashBar),
            b'|' => simple_kr(MOD_ALT, KeyboardUsage::KeyboardBackslashBar),
            b';' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardSemiColon),
            b':' => simple_kr(MOD_ALT, KeyboardUsage::KeyboardSemiColon),
            b'/' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardSlashQuestion),
            b'?' => simple_kr(MOD_ALT, KeyboardUsage::KeyboardSlashQuestion),
            b'\t' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardTab),
            b'\r' => simple_kr(MOD_NORM, KeyboardUsage::KeyboardEnter),
            127 => simple_kr(MOD_NORM, KeyboardUsage::KeyboardBackspace),
            _ => {
                log::error!("Unsupported character '{}'", ch);
                None
            }
        }
    }
}
