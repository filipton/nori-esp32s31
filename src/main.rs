//! A nori speaker on the ESP32-S31-Function-CoreBoard-1: joins the network (esp-hal-wifimanager, set up
//! over its access point or BLE), announces itself over mDNS, and plays what nori's deep cast sends it
//! through the board's ES8311 codec and speaker amplifier.

#![no_std]
#![no_main]

extern crate alloc;

mod button;
mod codec;
mod flac;
mod heap;
mod mdns;
mod mp3;
mod play;
mod psram;
mod receive;
mod screen;
mod setup;
mod stream;

use embassy_executor::Spawner;
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

/// The second core's stack (internal RAM: the core starts on it). A cover's decoder is on the heap.
const SCREENS_STACK: usize = 32 * 1024;

/// Sockets on the station's stack: DHCP, DNS, mDNS and the cast's stream.
const SOCKETS: usize = 4;

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()));
    // Internal RAM first: the BLE controller's memory comes from the plain heap and must be there.
    // Big buffers ask for PSRAM by name (`psram`).
    esp_alloc::heap_allocator!(size: 160 * 1024);
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    esp_println::logger::init_logger_from_env();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    let id = alloc::format!("{:012x}", esp_hal_wifimanager::get_efuse_mac());
    let setup: &'static str = alloc::format!("nori-{}", &id[..6]).leak();
    let stream = stream::Stream::new();

    // The screens on the second core: decoding a cover and drawing take longer than the audio
    // output holds.
    let screen_pins = screen::Pins {
        spi: peripherals.SPI2,
        dma: peripherals.DMA_AXI_CH0,
        sck: peripherals.GPIO20,
        mosi: peripherals.GPIO21,
        dc: peripherals.GPIO22,
        cs: peripherals.GPIO23,
        rst: peripherals.GPIO24,
    };
    static SCREENS: StaticCell<esp_hal::system::Stack<SCREENS_STACK>> = StaticCell::new();
    esp_rtos::start_second_core(peripherals.CPU_CTRL, SCREENS.init(esp_hal::system::Stack::new()), move || {
        static EXECUTOR: StaticCell<esp_rtos::embassy::Executor> = StaticCell::new();
        EXECUTOR.init(esp_rtos::embassy::Executor::new()).run(|spawner| {
            spawner.spawn(screen::run(screen_pins, stream).expect("screen"));
        });
    });

    let output = codec::Output::new(codec::Pins {
        i2s: peripherals.I2S0,
        dma: peripherals.DMA_CH0,
        i2c: peripherals.I2C0,
        mclk: peripherals.GPIO52,
        bclk: peripherals.GPIO53,
        ws: peripherals.GPIO55,
        dout: peripherals.GPIO56,
        scl: peripherals.GPIO50,
        sda: peripherals.GPIO51,
        amp: peripherals.GPIO57,
    })
    .await;

    let nvs = esp_hal_wifimanager::Nvs::new(peripherals.FLASH).expect("nvs");
    let mut settings = esp_hal_wifimanager::WmSettings::default();
    settings.ssid.clear();
    _ = core::fmt::write(&mut settings.ssid, format_args!("{setup}"));

    static RESOURCES: StaticCell<embassy_net::StackResources<SOCKETS>> = StaticCell::new();
    let wifi = esp_hal_wifimanager::init_wm_with(settings, &spawner, Some(nvs), peripherals.WIFI, peripherals.BT, RESOURCES.init(embassy_net::StackResources::new()))
        .await
        .expect("wifi");
    let stack = wifi.stack();
    stack.wait_config_up().await;
    log::info!("on the network at {}", wifi.ip());

    let name = setup::name(wifi.data());
    stream.with(|s| s.net = stream::Net::Ready(name));
    stream.changed();
    spawner.spawn(mdns::announce(stack, id, name).expect("mdns"));
    spawner.spawn(receive::serve(stack, stream).expect("receive"));
    spawner.spawn(button::watch(peripherals.GPIO61, stream).expect("button"));
    log::info!("internal RAM free: {}, PSRAM free: {}", esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::Internal.into()), esp_alloc::HEAP.free_caps(esp_alloc::MemoryCapability::External.into()));
    play::play(stream, output).await;
}
