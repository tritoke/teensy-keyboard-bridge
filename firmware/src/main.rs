//! Demonstrates a USB keypress using RTIC.
//!
//! Flash your board with this example. Your device will occasionally
//! send some kind of keypress to your host.

#![no_std]
#![no_main]

use teensy4_panic as _;

#[rtic::app(device = teensy4_bsp, peripherals = false)]
mod app {
    use heapless::spsc::Queue;
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
        descriptor::{KeyboardReport, SerializedDescriptor as _},
        hid_class::HIDClass,
    };

    /// Change me if you want to play with a full-speed USB device.
    const SPEED: Speed = Speed::High;
    /// https://pid.codes/1209/C00B/
    const VID_PID: UsbVidPid = UsbVidPid(0x1209, 0xC00B);
    const PRODUCT: &str = "teensy-keyboard-bridge";
    /// How frequently should we poll the logger?
    const LPUART_POLL_INTERVAL_MS: u32 = board::PERCLK_FREQUENCY / 1_000 * 100;
    /// The USB GPT timer we use to (infrequently) send mouse updates.
    const GPT_INSTANCE: gpt::Instance = gpt::Instance::Gpt0;
    /// How frequently should we push keyboard updates to the host?
    const KEYBOARD_UPDATE_INTERVAL_MS: u32 = 1;

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
        keys_to_press: Queue<KeyboardReport, 32>,
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
        // TODO: ? https://pid.codes/howto/
        let device = UsbDeviceBuilder::new(bus, VID_PID)
            .strings(&[usb_device::device::StringDescriptors::default().product(PRODUCT)])
            .unwrap()
            .device_class(usbd_serial::USB_CLASS_CDC)
            .max_packet_size_0(64)
            .unwrap()
            .build();

        (
            Shared {
                keys_to_press: Queue::new(),
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

        if let Some(key) = keys_to_press.lock(|keys| {
            if keys.len() > 1 {
                // don't leave the buffer empty
                led.set_high().ok();
                keys.dequeue()
            } else {
                led.set_low().ok();
                keys.peek().copied()
            }
        }) {
            class.push_input(&key).ok();
        } else {
            // if we have received no keypresses return None
            class.push_input(&KeyboardReport::default()).ok();
        }
    }

    #[task(binds = LPUART2, local = [lpuart2, buf: heapless::Vec<u8, 32> = heapless::Vec::new()], shared = [keys_to_press], priority = 3)]
    fn lpuart2_interrupt(ctx: lpuart2_interrupt::Context) {
        use lpuart::Status;
        let lpuart2 = ctx.local.lpuart2;
        let mut keys_to_press = ctx.shared.keys_to_press;
        let buf = ctx.local.buf;

        let status = lpuart2.status();
        lpuart2.clear_status(Status::W1C);

        if status.contains(Status::RECEIVE_FULL) {
            loop {
                let data = lpuart2.read_data();
                if data.flags().contains(lpuart::ReadFlags::RXEMPT) {
                    break;
                }

                let byte = u8::from(data);
                let is_full = buf.push(byte).is_err();

                // if were full something's gone wrong, just bail
                if is_full {
                    buf.clear();
                }

                // end of COBS packet wheeee
                if byte == 0 {
                    let maybe_report = postcard::from_bytes_cobs::<
                        '_,
                        shared::WhyNoDeriveDeserializeManSadFaceHere,
                    >(buf.as_mut_slice());

                    if let Ok(report) = maybe_report {
                        keys_to_press.lock(|keys| keys.enqueue(report.into()).ok());
                    }

                    buf.clear()
                }
            }
        }
    }
}
