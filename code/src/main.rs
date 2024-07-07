#![no_std]
#![no_main]

use panic_probe as _;
use defmt_rtt as _;

use stm32f4xx_hal as hal;

use core::cell::{OnceCell, RefCell};
use cortex_m::interrupt::Mutex;
use cortex_m_rt::entry;
use defmt::{error, info};
use fugit::{HertzU32, Rate};
use heapless::spsc::{self, Consumer, Producer};

use hal::{
    pac, prelude::*,
    gpio::{Output, PushPull, PC13},
    interrupt,
    otg_fs::{UsbBus, USB},
    pac::TIM1,
    timer::*,
};

use usb_device::{bus::UsbBusAllocator, prelude::*};
use usbd_audio::{AudioClass, AudioClassBuilder, StreamConfig, TerminalType};


const PWM_FREQ: HertzU32 = Rate::<u32, 1, 1>::kHz(40);
const AUDIO_QUEUE_SIZE: usize = 4096;
// The amount of samples with the value of 0 received before turning off the output
const NO_SIGNAL_SAMPLES: usize = 20000;

type PWMType = PwmHz<TIM1, (ChannelBuilder<TIM1, 0, true>, ChannelBuilder<TIM1, 1, true>)>;

static G_PWM: Mutex<RefCell<Option<PWMType>>> = Mutex::new(RefCell::new(None));
static G_MAX_DUTY: Mutex<OnceCell<u16>> = Mutex::new(OnceCell::new());

static G_USB_DEVICE: Mutex<RefCell<Option<UsbDevice<UsbBus<USB>>>>> =
    Mutex::new(RefCell::new(None));
static G_USB_AUDIO: Mutex<RefCell<Option<AudioClass<'static, UsbBus<USB>>>>> =
    Mutex::new(RefCell::new(None));

static G_AUDIO_QUEUE_PROD: Mutex<RefCell<Option<Producer<'static, i16, AUDIO_QUEUE_SIZE>>>> =
    Mutex::new(RefCell::new(None));
static G_AUDIO_QUEUE_CONS: Mutex<RefCell<Option<Consumer<'static, i16, AUDIO_QUEUE_SIZE>>>> =
    Mutex::new(RefCell::new(None));

static LED: Mutex<RefCell<Option<PC13<Output<PushPull>>>>> = Mutex::new(RefCell::new(None));

fn set_output_state(pwm: &mut PWMType, channel: Channel, enabled: bool) {
    // The output is disabled when the control signals are complementary
    // The primary output is ActiveHigh for C1 and ActiveLow for C2
    pwm.set_complementary_polarity(
        channel,
        match channel {
            Channel::C1 if enabled => Polarity::ActiveLow,
            Channel::C1 if !enabled => Polarity::ActiveHigh,
            Channel::C2 if enabled => Polarity::ActiveHigh,
            Channel::C2 if !enabled => Polarity::ActiveLow,
            _ => panic!("Only C1 and C2 are supported"),
        },
    );
}

#[entry]
fn main() -> ! {
    static mut EP_MEMORY: [u32; 1024] = [0; 1024];
    static mut USB_BUS: Option<UsbBusAllocator<stm32f4xx_hal::otg_fs::UsbBusType>> = None;
    static mut AUDIO_QUEUE: spsc::Queue<i16, AUDIO_QUEUE_SIZE> = spsc::Queue::new();

    let mut c = cortex_m::Peripherals::take().unwrap();
    let p = pac::Peripherals::take().unwrap();

    let gpioa = p.GPIOA.split();
    let gpiob = p.GPIOB.split();
    let gpioc = p.GPIOC.split();

    let rcc = p.RCC.constrain();

    let clocks = rcc
        .cfgr
        .use_hse(25u32.MHz())
        .sysclk(96.MHz())
        .i2s_clk(61440.kHz())
        .require_pll48clk()
        .freeze();

    let led = gpioc.pc13.into_push_pull_output();

    cortex_m::interrupt::free(|cs| {
        LED.borrow(cs).replace(Some(led));
    });

    let pwm_channels = (
        Channel1::new(gpioa.pa8).with_complementary(gpiob.pb13),
        Channel2::new(gpioa.pa9).with_complementary(gpiob.pb14),
    );
    let mut pwm = p.TIM1.pwm_hz(pwm_channels, PWM_FREQ, &clocks);

    // Disable the output on power up
    set_output_state(&mut pwm, Channel::C1, false);
    set_output_state(&mut pwm, Channel::C2, false);

    pwm.set_polarity(Channel::C1, Polarity::ActiveHigh);
    pwm.enable(Channel::C1);
    pwm.enable_complementary(Channel::C1);

    pwm.set_polarity(Channel::C2, Polarity::ActiveLow);
    pwm.enable(Channel::C2);
    pwm.enable_complementary(Channel::C2);

    let max_duty = pwm.get_max_duty();
    info!("Max duty: {}", max_duty);
    pwm.set_duty(Channel::C1, max_duty / 2);
    pwm.set_duty(Channel::C2, max_duty / 2);
    pwm.listen(Event::C1);

    // Audio
    let (prod, cons) = AUDIO_QUEUE.split();

    let usb = USB::new(
        (p.OTG_FS_GLOBAL, p.OTG_FS_DEVICE, p.OTG_FS_PWRCLK),
        (gpioa.pa11, gpioa.pa12),
        &clocks,
    );

    *USB_BUS = Some(stm32f4xx_hal::otg_fs::UsbBusType::new(usb, EP_MEMORY));
    let usb_bus = USB_BUS.as_ref().unwrap();

    cortex_m::interrupt::free(|cs| {
        G_AUDIO_QUEUE_PROD.borrow(cs).replace(Some(prod));
        G_AUDIO_QUEUE_CONS.borrow(cs).replace(Some(cons));

        G_USB_DEVICE.borrow(cs).replace(Some(
            UsbDeviceBuilder::new(usb_bus, UsbVidPid(0x16c0, 0x27e0))
                .strings(&[StringDescriptors::default()
                    .manufacturer("Orange_Murker")
                    .product("Parametric Speaker")])
                .unwrap()
                .build(),
        ));

        G_USB_AUDIO.borrow(cs).replace(Some(
            AudioClassBuilder::new()
                .output(
                    StreamConfig::new_discrete(
                        usbd_audio::Format::S16le,
                        1,
                        &[40000],
                        TerminalType::OutSpeaker,
                    )
                    .unwrap(),
                )
                .build(usb_bus)
                .unwrap(),
        ));
    });

    cortex_m::interrupt::free(|cs| {
        G_PWM.borrow(cs).replace(Some(pwm));
        G_MAX_DUTY.borrow(cs).get_or_init(|| max_duty);
    });

    unsafe {
        c.NVIC.set_priority(pac::Interrupt::OTG_FS, 16);

        cortex_m::peripheral::NVIC::unmask(pac::Interrupt::TIM1_CC);
        cortex_m::peripheral::NVIC::unmask(pac::Interrupt::OTG_FS);
    }
    info!(
        "OTG interrupt priority: {}",
        cortex_m::peripheral::NVIC::get_priority(pac::Interrupt::OTG_FS)
    );

    loop {}
}

#[interrupt]
fn TIM1_CC() {
    static mut PWM: Option<PWMType> = None;
    static mut MAX_DUTY: Option<u16> = None;
    static mut AUDIO_QUEUE_CONS: Option<Consumer<'static, i16, AUDIO_QUEUE_SIZE>> = None;
    // Disable the output at the start
    static mut ZERO_COUNT: usize = NO_SIGNAL_SAMPLES + 1;

    let pwm =
        PWM.get_or_insert_with(|| cortex_m::interrupt::free(|cs| G_PWM.borrow(cs).take().unwrap()));
    let max_duty = MAX_DUTY.get_or_insert_with(|| {
        cortex_m::interrupt::free(|cs| *G_MAX_DUTY.borrow(cs).get().unwrap())
    });
    let queue = AUDIO_QUEUE_CONS.get_or_insert_with(|| {
        cortex_m::interrupt::free(|cs| G_AUDIO_QUEUE_CONS.borrow(cs).take().unwrap())
    });

    pwm.clear_flags(Flag::C1);

    // cortex_m::interrupt::free(|cs| {
    //     let mut led = LED.borrow(cs).take().unwrap();
    //     led.set_high();
    //     LED.borrow(cs).replace(Some(led));
    // });

    const AMPLIFY: i32 = 0000;

    let min = i16::MIN as i32 + AMPLIFY;
    let max = i16::MAX as i32 - AMPLIFY;

    let value = if let Some(value) = queue.dequeue() {
        value
    } else {
        error!("Underrun");
        0
    };

    if value == 0 {
        *ZERO_COUNT = ZERO_COUNT.saturating_add(1);
        // Disable the output when there is no signal
        if *ZERO_COUNT > NO_SIGNAL_SAMPLES {
            set_output_state(pwm, Channel::C1, false);
            set_output_state(pwm, Channel::C2, false);
        }
    } else {
        // Re-enable the output
        set_output_state(pwm, Channel::C1, true);
        set_output_state(pwm, Channel::C2, true);
        *ZERO_COUNT = 0;
    }

    let duty = ((*max_duty as i32 * (value as i32 - min)) / (max - min)) as u16;

    // info!("Duty: {}", duty);

    pwm.set_duty(Channel::C1, duty as u16);
    pwm.set_duty(Channel::C2, duty as u16);

    // cortex_m::interrupt::free(|cs| {
    //     let mut led = LED.borrow(cs).take().unwrap();
    //     led.set_low();
    //     LED.borrow(cs).replace(Some(led));
    // });
}

#[interrupt]
fn OTG_FS() {
    static mut USB_DEVICE: Option<UsbDevice<UsbBus<USB>>> = None;
    static mut USB_AUDIO: Option<AudioClass<'static, UsbBus<USB>>> = None;
    static mut AUDIO_QUEUE_PROD: Option<Producer<'static, i16, AUDIO_QUEUE_SIZE>> = None;

    let usb_dev = USB_DEVICE.get_or_insert_with(|| {
        cortex_m::interrupt::free(|cs| G_USB_DEVICE.borrow(cs).take().unwrap())
    });

    let usb_audio = USB_AUDIO.get_or_insert_with(|| {
        cortex_m::interrupt::free(|cs| G_USB_AUDIO.borrow(cs).take().unwrap())
    });

    let queue = AUDIO_QUEUE_PROD.get_or_insert_with(|| {
        cortex_m::interrupt::free(|cs| G_AUDIO_QUEUE_PROD.borrow(cs).take().unwrap())
    });

    if usb_dev.poll(&mut [usb_audio]) {
        let mut buf: [u8; 1024] = [0u8; 1024];
        if let Ok(len) = usb_audio.read(&mut buf) {
            let data = &buf[0..len];
            // info!("{}", len);
            for x in data.chunks_exact(2) {
                let val = i16::from_le_bytes(
                    x.try_into()
                        .expect("Should not panic because chunks are always 2 bytes"),
                );
                // info!("Val: {}", val);
                if queue.enqueue(val).is_err() {
                    error!("Overrun");
                }
            }
        }
    }
}
