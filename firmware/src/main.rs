// Simple keyboard firmware. Inspired by the RustyKeys project:
// https://github.com/KOBA789/rusty-keys/blob/main/firmware/keyboard/src/bin/simple.rs

#![no_main]
#![no_std]

use core::convert::Infallible;
use cortex_m::{delay::Delay};
use defmt::{error, info, warn};
use defmt_rtt as _;
use fugit::MicrosDurationU32;
use embedded_hal::{
    digital::v2::{InputPin, OutputPin},
    timer::CountDown,
};
// use panic_reset as _;
use panic_probe as _;
use rp2040_hal::{pac::{self, interrupt}, usb::{self, UsbBus}, Clock, Watchdog};
use usb_device::{bus::UsbBusAllocator, device::UsbDeviceBuilder, prelude::UsbVidPid, UsbError};
use usbd_hid::{
    descriptor::KeyboardReport,
    hid_class::{
        HIDClass, HidClassSettings, HidCountryCode, HidProtocol, HidSubClass, ProtocolModeConfig,
    },
};
use usb_device::{class_prelude::*, prelude::*};
use rp2040_hal::prelude::*;
use usbd_hid::descriptor::generator_prelude::*;

/// The linker will place this boot block at the start of our program image. We
/// need this to help the ROM bootloader get our code up and running.
#[link_section = ".boot2"]
#[used]
pub static BOOT2: [u8; 256] = rp2040_boot2::BOOT_LOADER_W25Q080;

mod hid_descriptor;
mod key_codes;
mod key_mapping;

const NUM_COLS: usize = 14;
const NUM_ROWS: usize = 6;

const EXTERNAL_CRYSTAL_FREQUENCY_HZ: u32 = 12_000_000;

/// The USB Device Driver (shared with the interrupt).
static mut USB_DEVICE: Option<UsbDevice<usb::UsbBus>> = None;

/// The USB Bus Driver (shared with the interrupt).
static mut USB_BUS: Option<UsbBusAllocator<usb::UsbBus>> = None;

/// The USB Human Interface Device Driver (shared with the interrupt).
static mut USB_HID: Option<HIDClass<usb::UsbBus>> = None;

/// The latest keyboard report for responding to USB interrupts.
static mut KEYBOARD_REPORT: Option<KeyboardReport> = None;

#[defmt::panic_handler]
fn panic() -> ! {
    cortex_m::asm::udf()
}

#[cortex_m_rt::entry]
fn main() -> ! {
    info!("Start of main()");
    let mut pac = pac::Peripherals::take().unwrap();
    let mut core = pac::CorePeripherals::take().unwrap();

    let mut watchdog = Watchdog::new(pac.WATCHDOG);

    let clocks = rp2040_hal::clocks::init_clocks_and_plls(
        EXTERNAL_CRYSTAL_FREQUENCY_HZ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    // Setup USB
    let force_vbus_detect_bit = true;
    let usb_bus = UsbBus::new(
        pac.USBCTRL_REGS,
        pac.USBCTRL_DPRAM,
        clocks.usb_clock,
        force_vbus_detect_bit,
        &mut pac.RESETS,
    );

    let bus_allocator = UsbBusAllocator::new(usb_bus);
    unsafe {
        // Note (safety): This is safe as interrupts haven't been started yet
        USB_BUS = Some(bus_allocator);
    }
    // Grab a reference to the USB Bus allocator. We are promising to the
    // compiler not to take mutable access to this global variable whilst this
    // reference exists!
    let bus_ref = unsafe { USB_BUS.as_ref().unwrap() };

    // Note - Going lower than this requires switch debouncing.
    let poll_ms = 8;
    let mut hid_endpoint = HIDClass::new_with_settings(
        bus_ref,
        hid_descriptor::KEYBOARD_REPORT_DESCRIPTOR,
        poll_ms,
        HidClassSettings {
            subclass: HidSubClass::NoSubClass,
            protocol: HidProtocol::Keyboard,
            config: ProtocolModeConfig::ForceReport,
            // locale: HidCountryCode::NotSupported,
            locale: HidCountryCode::US,
        },
    );
    unsafe {
        // Note (safety): This is safe as interrupts haven't been started yet.
        USB_HID = Some(hid_endpoint);
    }

    info!("USB initialized");

    // https://github.com/obdev/v-usb/blob/7a28fdc685952412dad2b8842429127bc1cf9fa7/usbdrv/USB-IDs-for-free.txt#L128
    let mut keyboard_usb_device = UsbDeviceBuilder::new(bus_ref, UsbVidPid(0x16c0, 0x27db))
        .manufacturer("bschwind")
        .product("key ripper")
        .build();
    unsafe {
        // Note (safety): This is safe as interrupts haven't been started yet
        USB_DEVICE = Some(keyboard_usb_device);
    }

    // Get the GPIO peripherals.
    let sio = rp2040_hal::Sio::new(pac.SIO);

    let pins =
        rp2040_hal::gpio::Pins::new(pac.IO_BANK0, pac.PADS_BANK0, sio.gpio_bank0, &mut pac.RESETS);

    // Set up keyboard matrix pins.
    let rows: &[&dyn InputPin<Error = Infallible>] = &[
        &pins.gpio26.into_pull_down_input(),
        &pins.gpio25.into_pull_down_input(),
        &pins.gpio27.into_pull_down_input(),
        &pins.gpio28.into_pull_down_input(),
        &pins.gpio15.into_pull_down_input(),
        &pins.gpio24.into_pull_down_input(),
    ];

    let cols: &mut [&mut dyn OutputPin<Error = Infallible>] = &mut [
        &mut pins.gpio29.into_push_pull_output(),
        &mut pins.gpio16.into_push_pull_output(),
        &mut pins.gpio17.into_push_pull_output(),
        &mut pins.gpio18.into_push_pull_output(),
        &mut pins.gpio9.into_push_pull_output(),
        &mut pins.gpio10.into_push_pull_output(),
        &mut pins.gpio19.into_push_pull_output(),
        &mut pins.gpio11.into_push_pull_output(),
        &mut pins.gpio12.into_push_pull_output(),
        &mut pins.gpio13.into_push_pull_output(),
        &mut pins.gpio14.into_push_pull_output(),
        &mut pins.gpio20.into_push_pull_output(),
        &mut pins.gpio22.into_push_pull_output(),
        &mut pins.gpio23.into_push_pull_output(),
    ];

    // Timer-based resources.
    let mut delay = cortex_m::delay::Delay::new(core.SYST, clocks.system_clock.freq().to_Hz());

    let timer = rp2040_hal::Timer::new(pac.TIMER, &mut pac.RESETS);
    let mut scan_countdown = timer.count_down();

    // Start on a 500ms countdown so the USB endpoint writes don't block.
    scan_countdown.start(MicrosDurationU32::millis(500));

    info!("Start main loop");

    let matrix = scan_keys(rows, cols, &mut delay);

    // If the Escape key is pressed during power-on, we should go into bootloader mode.
    if matrix[0][0] {
        let gpio_activity_pin_mask = 0;
        let disable_interface_mask = 0;
        rp2040_hal::rom_data::reset_to_usb_boot(gpio_activity_pin_mask, disable_interface_mask);
    }

    info!("setting interrupt");
    unsafe {
        // core.NVIC.set_priority(pac::Interrupt::USBCTRL_IRQ, 1);
        pac::NVIC::unmask(pac::Interrupt::USBCTRL_IRQ);
    }
    info!("interrupt set.");
    // Main keyboard polling loop.
    loop {
        // keyboard_usb_device.poll(&mut [&mut hid_endpoint]);

        if scan_countdown.wait().is_ok() {
            // Scan the keys and send a report.
            let matrix = scan_keys(rows, cols, &mut delay);
            let report = report_from_matrix(&matrix);

            match push_mouse_movement(report) {
                Ok(_) => {
                    scan_countdown.start(MicrosDurationU32::millis(8));
                },
                Err(err) => match err {
                    UsbError::WouldBlock => warn!("UsbError::WouldBlock"),
                    UsbError::ParseError => error!("UsbError::ParseError"),
                    UsbError::BufferOverflow => error!("UsbError::BufferOverflow"),
                    UsbError::EndpointOverflow => error!("UsbError::EndpointOverflow"),
                    UsbError::EndpointMemoryOverflow => error!("UsbError::EndpointMemoryOverflow"),
                    UsbError::InvalidEndpoint => error!("UsbError::InvalidEndpoint"),
                    UsbError::Unsupported => error!("UsbError::Unsupported"),
                    UsbError::InvalidState => error!("UsbError::InvalidState"),
                },
            }
        }
    }
}

fn push_mouse_movement(report: KeyboardReport) -> Result<usize, usb_device::UsbError> {
    critical_section::with(|_| unsafe {
        // Now interrupts are disabled, grab the global variable and, if
        // available, send it a HID report
        USB_HID.as_mut().map(|hid| {
            hid.push_input(&report);
            hid.pull_raw_output(&mut [0; 64])
        })
    })
    .unwrap()
}

fn scan_keys(
    rows: &[&dyn InputPin<Error = Infallible>],
    columns: &mut [&mut dyn embedded_hal::digital::v2::OutputPin<Error = Infallible>],
    delay: &mut Delay,
) -> [[bool; NUM_ROWS]; NUM_COLS] {
    let mut matrix = [[false; NUM_ROWS]; NUM_COLS];

    for (gpio_col, matrix_col) in columns.iter_mut().zip(matrix.iter_mut()) {
        gpio_col.set_high().unwrap();
        delay.delay_us(10);

        for (gpio_row, matrix_row) in rows.iter().zip(matrix_col.iter_mut()) {
            *matrix_row = gpio_row.is_high().unwrap();
        }

        gpio_col.set_low().unwrap();
        delay.delay_us(10);
    }

    matrix
}

fn report_from_matrix(matrix: &[[bool; NUM_ROWS]; NUM_COLS]) -> KeyboardReport {
    let mut keycodes = [0u8; 6];
    let mut keycode_index = 0;
    let mut modifier = 0;

    let mut push_keycode = |key| {
        if keycode_index < keycodes.len() {
            keycodes[keycode_index] = key;
            keycode_index += 1;
        }
    };

    let layer_mapping = if matrix[0][5] {
        key_mapping::FN_LAYER_MAPPING
    } else {
        key_mapping::NORMAL_LAYER_MAPPING
    };

    for (matrix_column, mapping_column) in matrix.iter().zip(layer_mapping) {
        for (key_pressed, mapping_row) in matrix_column.iter().zip(mapping_column) {
            if *key_pressed {
                if let Some(bitmask) = mapping_row.modifier_bitmask() {
                    modifier |= bitmask;
                } else {
                    push_keycode(mapping_row as u8);
                }
            }
        }
    }

    KeyboardReport { modifier, reserved: 0, leds: 0, keycodes }
}

#[allow(non_snake_case)]
#[interrupt]
unsafe fn USBCTRL_IRQ() {
    info!("usb irq");
    // Handle USB request
    let usb_dev = USB_DEVICE.as_mut().unwrap();
    let usb_hid = USB_HID.as_mut().unwrap();
    usb_dev.poll(&mut [usb_hid]);
}