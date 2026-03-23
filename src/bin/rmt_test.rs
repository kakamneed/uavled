#![no_std]
#![no_main]

use embedded_hal::delay::DelayNs;
use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    gpio::Level,
    rmt::{PulseCode, Rmt, TxChannel, TxChannelConfig, TxChannelCreator},
    time::Rate,
};
use esp_println::println;

const NUM_LEDS: usize = 8;
const RES_US: u32 = 500;

// RMT @ 80MHz, 1 tick = 12.5ns
const SK_T0H: u16 = 24; // 300ns
const SK_T0L: u16 = 72; // 900ns
const SK_T1H: u16 = 48; // 600ns
const SK_T1L: u16 = 48; // 600ns

// SK6812 RGBW: 32bit/LED (4 字节: G, R, B, W)
const BYTES_PER_LED: usize = 4;
const GUARD_LEDS: usize = 4;
const RESET_PULSES: usize = 2;
const RMT_BUF_LEN: usize = (NUM_LEDS + GUARD_LEDS) * 32 + RESET_PULSES + 1;

static mut LED_BUF: [u8; NUM_LEDS * BYTES_PER_LED] = [0u8; NUM_LEDS * BYTES_PER_LED];

fn encode_sk6812(data: &[u8], buf: &mut [u32; RMT_BUF_LEN]) {
    let mut idx = 0;
    let max_bytes = NUM_LEDS * BYTES_PER_LED;
    let len = if data.len() < max_bytes { data.len() } else { max_bytes };
    for i in 0..len {
        let mut b = data[i];
        for _ in 0..8 {
            buf[idx] = if (b & 0x80) != 0 {
                PulseCode::new(Level::High, SK_T1H, Level::Low, SK_T1L)
            } else {
                PulseCode::new(Level::High, SK_T0H, Level::Low, SK_T0L)
            };
            b <<= 1;
            idx += 1;
        }
    }
    // 追加 GUARD_LEDS 个全黑 LED
    for _ in 0..(GUARD_LEDS * BYTES_PER_LED) {
        for _ in 0..8 {
            buf[idx] = PulseCode::new(Level::High, SK_T0H, Level::Low, SK_T0L);
            idx += 1;
        }
    }
    for _ in 0..RESET_PULSES {
        buf[idx] = PulseCode::new(Level::Low, 24000, Level::Low, 24000);
        idx += 1;
    }
    buf[idx] = PulseCode::empty();
}

type LedChannel = esp_hal::rmt::Channel<esp_hal::Blocking, 0>;

fn send_leds(channel: LedChannel, delay: &mut Delay, data: &[u8]) -> LedChannel {
    let mut rmt_buf = [0u32; RMT_BUF_LEN];
    encode_sk6812(data, &mut rmt_buf);
    let tx = channel.transmit(&rmt_buf).unwrap();
    let ch = tx.wait().unwrap();
    delay.delay_us(RES_US);
    ch
}

// =============================================
// 纯 Rust 灯效（不依赖 JS）
// =============================================

/// 设置单个 LED (GRBW 顺序 — SK6812 RGBW, W=0)
fn set_led(buf: &mut [u8], idx: usize, r: u8, g: u8, b: u8) {
    let o = idx * BYTES_PER_LED;
    buf[o] = g;
    buf[o + 1] = r;
    buf[o + 2] = b;
    buf[o + 3] = 0; // W 通道
}

/// 设置全部 LED
fn set_all(buf: &mut [u8], r: u8, g: u8, b: u8) {
    for i in 0..NUM_LEDS {
        set_led(buf, i, r, g, b);
    }
}

/// 纯红
fn effect_red(buf: &mut [u8], _frame: u32) {
    set_all(buf, 255, 0, 0);
}

/// 追逐：单颗白灯逐颗移动
fn effect_chase(buf: &mut [u8], frame: u32) {
    set_all(buf, 0, 0, 0);
    set_led(buf, (frame as usize) % NUM_LEDS, 255, 255, 255);
}

/// 呼吸：紫色渐亮渐暗
fn effect_breathe(buf: &mut [u8], frame: u32) {
    let t = (frame % 128) as u8;
    let v = if t < 64 { t * 4 } else { (127 - t) * 4 };
    set_all(buf, v, 0, v);
}

/// 彩虹
fn effect_rainbow(buf: &mut [u8], frame: u32) {
    for i in 0..NUM_LEDS {
        let hue = ((frame as usize * 2 + i * 32) % 768) as i32;
        let (r, g, b) = if hue < 256 {
            (255 - hue, hue, 0)
        } else if hue < 512 {
            let h = hue - 256;
            (0, 255 - h, h)
        } else {
            let h = hue - 512;
            (h, 0, 255 - h)
        };
        set_led(buf, i, r as u8, g as u8, b as u8);
    }
}

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let mut delay = Delay::new();

    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let rmt_channel = rmt.channel0.configure(
        peripherals.GPIO10,
        TxChannelConfig::default()
            .with_clk_divider(1)
            .with_idle_output_level(Level::Low)
            .with_idle_output(true)
            .with_carrier_modulation(false),
    ).unwrap();
    let mut channel: Option<LedChannel> = Some(rmt_channel);

    println!("=== RMT 纯硬件灯效测试 ===");

    // ── 阶段0：单色诊断（每色2秒，确认 RMT 基本功能）──
    let diagnostics: [(&str, u8, u8, u8); 6] = [
        ("全灭(黑)", 0, 0, 0),
        ("纯红",     255, 0, 0),
        ("纯绿",     0, 255, 0),
        ("纯蓝",     0, 0, 255),
        ("纯白",     255, 255, 255),
        ("暗红(1)",  1, 0, 0),  // 只有 R 最低位为1，验证 bit 分辨能力
    ];
    for (name, r, g, b) in &diagnostics {
        println!("[DIAG] {} R={} G={} B={}", name, r, g, b);
        let buf = unsafe { &mut LED_BUF };
        set_all(buf, *r, *g, *b);
        // 发送3次确保稳定锁存
        for _ in 0..3 {
            let ch = channel.take().unwrap();
            channel = Some(send_leds(ch, &mut delay, unsafe { &LED_BUF }));
            delay.delay_ms(16u32);
        }
        // 打印实际 GRB buffer 内容
        let raw = unsafe { &LED_BUF };
        println!("  GRB buf[0..3] = [{}, {}, {}]", raw[0], raw[1], raw[2]);
        delay.delay_ms(2000u32);
    }
    println!("[DIAG] 单色诊断完成");

    let effects: [(&str, fn(&mut [u8], u32)); 4] = [
        ("纯红", effect_red),
        ("追逐", effect_chase),
        ("呼吸", effect_breathe),
        ("彩虹", effect_rainbow),
    ];

    loop {
        for (name, effect_fn) in &effects {
            println!("[TEST] {} — 开始 (180帧 ≈ 3s)", name);
            for frame in 0..180u32 {
                let buf = unsafe { &mut LED_BUF };
                effect_fn(buf, frame);
                let ch = channel.take().unwrap();
                channel = Some(send_leds(ch, &mut delay, unsafe { &LED_BUF }));
                if frame == 0 {
                    // 打印第一帧前4颗 LED
                    let b = unsafe { &LED_BUF };
                    for i in 0..4.min(NUM_LEDS) {
                        println!("  led[{}] R={} G={} B={}", i, b[i*3+1], b[i*3], b[i*3+2]);
                    }
                }
                delay.delay_ms(16u32);
            }
            println!("[TEST] {} — 完成", name);
        }
        println!("=== 循环重播 ===");
    }
}
