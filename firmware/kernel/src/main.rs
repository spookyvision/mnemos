#![no_main]
#![no_std]

use kernel as _; // global logger + panicking-behavior + memory layout

#[rtic::app(device = nrf52840_hal::pac, dispatchers = [SWI0_EGU0])]
mod app {

    use cortex_m::singleton;
    use defmt::unwrap;
    use groundhog_nrf52::GlobalRollingTimer;
    use nrf52840_hal::{
        clocks::{ExternalOscillator, Internal, LfOscStopped},
        pac::TIMER0,
        usbd::{UsbPeripheral, Usbd},
        Clocks,
    };
    use kernel::{
        alloc::HEAP,
        monotonic::{MonoTimer},
        drivers::usb_serial::{UsbUartParts, setup_usb_uart, UsbUartIsr, enable_usb_interrupts},
        syscall::{syscall_clear, try_syscall, try_recv_syscall, SysCallRequest, SysCallSuccess},
    };
    use usb_device::{
        class_prelude::UsbBusAllocator,
        device::{UsbDeviceBuilder, UsbVidPid},
    };
    use usbd_serial::{SerialPort, USB_CLASS_CDC};
    use groundhog::RollingTimer;

    #[monotonic(binds = TIMER0, default = true)]
    type Monotonic = MonoTimer<TIMER0>;

    #[shared]
    struct Shared {}

    #[local]
    struct Local {
        usb_isr: UsbUartIsr,
        machine: kernel::traits::Machine,
    }

    #[init]
    fn init(cx: init::Context) -> (Shared, Local, init::Monotonics) {
        let device = cx.device;

        // Setup clocks early in the process. We need this for USB later
        let clocks = Clocks::new(device.CLOCK);
        let clocks = clocks.enable_ext_hfosc();
        let clocks =
            unwrap!(singleton!(: Clocks<ExternalOscillator, Internal, LfOscStopped> = clocks));

        // Configure the monotonic timer, currently using TIMER0, a 32-bit, 1MHz timer
        let mono = Monotonic::new(device.TIMER0);

        // I am annoying, and prefer my own libraries.
        GlobalRollingTimer::init(device.TIMER1);

        // Setup the heap
        HEAP.init().ok();

        // Reset the syscall contents
        syscall_clear();

        // Before we give away the USB peripheral, enable the relevant interrupts
        enable_usb_interrupts(&device.USBD);

        let (usb_dev, usb_serial) = {
            let usb_bus = Usbd::new(UsbPeripheral::new(device.USBD, clocks));
            let usb_bus = defmt::unwrap!(singleton!(:UsbBusAllocator<Usbd<UsbPeripheral>> = usb_bus));

            let usb_serial = SerialPort::new(usb_bus);
            let usb_dev = UsbDeviceBuilder::new(usb_bus, UsbVidPid(0x16c0, 0x27dd))
                .manufacturer("OVAR Labs")
                .product("Anachro Pellegrino")
                // TODO: Use some kind of unique ID. This will probably require another singleton,
                // as the storage must be static. Probably heapless::String -> singleton!()
                .serial_number("ajm001")
                .device_class(USB_CLASS_CDC)
                .max_packet_size_0(64) // (makes control transfers 8x faster)
                .build();

            (usb_dev, usb_serial)
        };

        let mut hg = defmt::unwrap!(HEAP.try_lock());

        let UsbUartParts { isr, sys } = defmt::unwrap!(setup_usb_uart(usb_dev, usb_serial));
        let box_uart = defmt::unwrap!(hg.alloc_box(sys));
        let leak_uart = box_uart.leak();
        let to_uart: &'static mut dyn kernel::traits::Serial = leak_uart;

        let machine = kernel::traits::Machine {
            serial: to_uart,
        };

        (
            Shared {},
            Local {
                usb_isr: isr,
                machine,
            },
            init::Monotonics(mono),
        )
    }

    #[task(binds = SVCall, local = [machine], priority = 1)]
    fn svc(cx: svc::Context) {
        let machine = cx.local.machine;

        if let Ok(()) = try_recv_syscall(|req| {
            machine.handle_syscall(req)
        }) {
            // defmt::println!("Handled syscall!");
        }
    }

    #[task(binds = USBD, local = [usb_isr], priority = 2)]
    fn usb_tick(cx: usb_tick::Context) {
        cx.local.usb_isr.poll();
    }

    // TODO: I am currently polling the syscall interfaces in the idle function,
    // since I don't have syscalls yet. In the future, the `machine` will be given
    // to the SWI handler, and idle will basically just launch a program. I think.
    // Maybe idle will use SWIs too.
    #[idle]
    fn idle(cx: idle::Context) -> ! {
        defmt::println!("Hello, world!");
        let timer = GlobalRollingTimer::default();
        let mut last_mem = timer.get_ticks();

        // First, open Port 1
        let req = SysCallRequest::SerialOpenPort { port: 1 };
        defmt::unwrap!(try_syscall(req));

        let mut buf = [0u8; 128];

        loop {
            if timer.millis_since(last_mem) >= 1000 {
                if let Some(hg) = HEAP.try_lock() {
                    last_mem = timer.get_ticks();
                    let used = hg.used_space();
                    let free = hg.free_space();
                    defmt::println!("used: {=usize}, free: {=usize}", used, free);
                    defmt::println!("Syscalling!");
                }

                let req = SysCallRequest::SerialReceive {
                    port: 0,
                    dest_buf: buf.as_mut().into(),
                };
                match try_syscall(req) {
                    Ok(succ) => {
                        if let SysCallSuccess::DataReceived { dest_buf } = succ {
                            let dest = unsafe { dest_buf.to_slice_mut() };
                            if dest.len() > 0 {
                                defmt::println!("Sending port 1!");
                                let req2 = SysCallRequest::SerialSend {
                                    port: 1,
                                    src_buf: (&*dest).into(),
                                };
                                match try_syscall(req2) {
                                    Ok(SysCallSuccess::DataSent { remainder }) => {
                                        defmt::assert!(remainder.is_none());
                                    },
                                    Ok(_) => defmt::panic!(),
                                    Err(_) => {
                                        defmt::println!("Port0 -> Port1 send failed!")
                                    }
                                }
                            }
                        } else {
                            defmt::panic!("What?");
                        }
                    }
                    Err(()) => {
                        defmt::panic!("syscall failed!");
                    }
                }

            }
        }
    }
}
