#![no_std]
#![no_main]

extern crate alloc;

use alloc::{format, string::String};

// SK6812 RGBW reset 时间（加大余量防止溢出）
const RES_US: u32 = 500;
const NUM_LEDS: usize = 8;

// BLE 脚本接收缓冲区（固件内置 prelude，只接收用户脚本，通常 < 500B）
const SCRIPT_BUF_SIZE: usize = 2048;

// OTA 固件接收缓冲区（用于累积一个 Flash 写入单位）
const OTA_BUF_SIZE: usize = 256;

use bleps::{
    ad_structure::{
        create_advertising_data, AdStructure, BR_EDR_NOT_SUPPORTED, LE_GENERAL_DISCOVERABLE,
    },
    attribute_server::{AttributeServer, WorkResult},
    gatt, Ble, HciConnector,
};
use embedded_hal::delay::DelayNs;
use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    gpio::Level,
    rmt::{PulseCode, Rmt, TxChannel, TxChannelConfig, TxChannelCreator},
    rng::Rng,
    system::software_reset,
    time::{Instant, Rate},
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_wifi::{ble::controller::BleConnector, init as wifi_init};
use mquickjs::vm::types::Interpreter;
use mquickjs::{Context, FunctionBytecode, NativeFn, Value};

use esp_storage::FlashStorage;

// OTA 模块（来自 lib crate）
use uavled::ota;

// LED 数据缓冲区（GRBW 格式，SK6812 RGBW 32bit/LED）
const BYTES_PER_LED: usize = 4;
static mut LED_BUFFER: [u8; NUM_LEDS * BYTES_PER_LED] = [0u8; NUM_LEDS * BYTES_PER_LED];

// BLE 脚本接收状态
static mut SCRIPT_BUF: [u8; SCRIPT_BUF_SIZE] = [0u8; SCRIPT_BUF_SIZE];
static mut SCRIPT_LEN: usize = 0;
static mut SCRIPT_READY: bool = false;

// OTA 状态
static mut OTA_STATE: ota::OtaState = ota::OtaState::new();
static mut OTA_BUF: [u8; OTA_BUF_SIZE] = [0u8; OTA_BUF_SIZE];
static mut OTA_BUF_LEN: usize = 0;

// =============================================
// JS 原生函数：LED 控制
// =============================================

/// setLed(index, r, g, b) - 设置单个 LED 颜色
fn native_set_led(
    _interp: &mut Interpreter,
    _this: Value,
    args: &[Value],
) -> Result<Value, String> {
    let idx = args.first().and_then(|v| v.to_i32()).unwrap_or(-1);
    let r = args.get(1).and_then(|v| v.to_i32()).unwrap_or(0) as u8;
    let g = args.get(2).and_then(|v| v.to_i32()).unwrap_or(0) as u8;
    let b = args.get(3).and_then(|v| v.to_i32()).unwrap_or(0) as u8;

    if idx < 0 || idx >= NUM_LEDS as i32 {
        return Ok(Value::undefined());
    }

    unsafe {
        let o = idx as usize * BYTES_PER_LED;
        LED_BUFFER[o] = g;     // GRBW 格式 (SK6812 RGBW)
        LED_BUFFER[o + 1] = r;
        LED_BUFFER[o + 2] = b;
        LED_BUFFER[o + 3] = 0; // W
    }
    Ok(Value::undefined())
}

/// getLedCount() - 返回 LED 数量
fn native_get_led_count(
    _interp: &mut Interpreter,
    _this: Value,
    _args: &[Value],
) -> Result<Value, String> {
    Ok(Value::int(NUM_LEDS as i32))
}

/// setAll(r, g, b) - 设置所有 LED 为同一颜色
fn native_set_all(
    _interp: &mut Interpreter,
    _this: Value,
    args: &[Value],
) -> Result<Value, String> {
    let r = args.first().and_then(|v| v.to_i32()).unwrap_or(0) as u8;
    let g = args.get(1).and_then(|v| v.to_i32()).unwrap_or(0) as u8;
    let b = args.get(2).and_then(|v| v.to_i32()).unwrap_or(0) as u8;

    unsafe {
        for i in 0..NUM_LEDS {
            let o = i * BYTES_PER_LED;
            LED_BUFFER[o] = g;     // GRBW 格式 (SK6812 RGBW)
            LED_BUFFER[o + 1] = r;
            LED_BUFFER[o + 2] = b;
            LED_BUFFER[o + 3] = 0; // W
        }
    }
    Ok(Value::undefined())
}

// =============================================
// SK6812 硬件驱动（RMT 外设，硬件精确时序）
// =============================================
// RMT @ 80MHz, 1 tick = 12.5ns
// SK6812 时序：
//   T0H = 300ns = 24 ticks,  T0L = 900ns = 72 ticks
//   T1H = 600ns = 48 ticks,  T1L = 600ns = 48 ticks
//   RESET ≥ 80us

const SK_T0H: u16 = 24;
const SK_T0L: u16 = 72;
const SK_T1H: u16 = 48;
const SK_T1L: u16 = 48;

// 额外发送 GUARD_LEDS 个全黑 LED 数据，防止信号噪声点亮后续 LED
const GUARD_LEDS: usize = 4;
// (NUM_LEDS + GUARD_LEDS) × 32 bits + RESET 脉冲 + 终止符
const RESET_PULSES: usize = 2;
const RMT_BUF_LEN: usize = (NUM_LEDS + GUARD_LEDS) * 32 + RESET_PULSES + 1;

/// 将 LED 数据编码为 RMT PulseCode 数组
/// 实际 LED 数据后追加 GUARD_LEDS 个全黑 LED + RESET 脉冲
fn encode_sk6812(data: &[u8], buf: &mut [u32; RMT_BUF_LEN]) {
    let mut idx = 0;
    // 编码实际 LED 数据（严格限制 NUM_LEDS 个）
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
    // 追加 GUARD_LEDS 个全黑 LED (全 0 bit = T0H+T0L 脉冲)
    // 即使信号噪声让后续 LED 误锁存，也只会锁存到黑色
    for _ in 0..(GUARD_LEDS * BYTES_PER_LED) {
        for _ in 0..8 {
            buf[idx] = PulseCode::new(Level::High, SK_T0H, Level::Low, SK_T0L);
            idx += 1;
        }
    }
    // 硬件 RESET 脉冲
    for _ in 0..RESET_PULSES {
        buf[idx] = PulseCode::new(Level::Low, 24000, Level::Low, 24000);
        idx += 1;
    }
    buf[idx] = PulseCode::empty(); // 终止符
}

// =============================================
// JS 灯效测试脚本
// =============================================

/// 纯红：所有 LED 设为 (255, 0, 0)
const TEST_RED: &str = r#"
function update() { setAll(255, 0, 0); }
"#;

/// 追逐：frame 帧时只有 (frame % count) 号灯亮白色
const TEST_CHASE: &str = r#"
var frame = 0;
function update() {
    var count = getLedCount();
    var i = 0;
    while (i < count) {
        setLed(i, 0, 0, 0);
        i = i + 1;
    }
    setLed(frame % count, 255, 255, 255);
    frame = frame + 1;
}
"#;

/// 呼吸：全局亮度按正弦波变化（用整数近似）
const TEST_BREATHE: &str = r#"
var frame = 0;
function update() {
    var t = frame % 128;
    var v = 0;
    if (t < 64) { v = t * 4; } else { v = (127 - t) * 4; }
    setAll(v, 0, v);
    frame = frame + 1;
}
"#;

/// 彩虹（与默认脚本相同，验证 reset_user_state 后重新加载）
const TEST_RAINBOW: &str = r#"
var frame = 0;
function update() {
    var count = getLedCount();
    var i = 0;
    while (i < count) {
        var hue = (frame * 2 + i * 32) % 768;
        var r = 0; var g = 0; var b = 0;
        if (hue < 256) { r = 255 - hue; g = hue; }
        else if (hue < 512) { var h = hue - 256; g = 255 - h; b = h; }
        else { var h = hue - 512; b = 255 - h; r = h; }
        setLed(i, r, g, b);
        i = i + 1;
    }
    frame = frame + 1;
}
"#;

/// 打印一帧 LED buffer 的前几个 LED（GRB 存储，打印为 RGB）
fn print_led_frame(label: &str, frame: usize) {
    unsafe {
        let buf = &LED_BUFFER;
        // 打印前4个 LED，格式: [R,G,B]
        let mut out = [0u8; 4 * 12]; // 每个 LED 最多 "[255,255,255]" = 13 chars
        let _ = label;
        let n = if NUM_LEDS < 4 { NUM_LEDS } else { 4 };
        println!("  frame={} leds[0..{}]:", frame, n);
        for i in 0..n {
            // GRBW 存储
            let o = i * BYTES_PER_LED;
            let g = buf[o];
            let r = buf[o + 1];
            let b = buf[o + 2];
            println!("    led[{}] R={} G={} B={}", i, r, g, b);
        }
        let _ = out;
    }
}

type LedChannel = esp_hal::rmt::Channel<esp_hal::Blocking, 0>;

/// 发送 LED 数据（消耗 channel 所有权，返回 channel）
fn send_leds(channel: LedChannel, delay: &mut Delay, data: &[u8]) -> LedChannel {
    let mut rmt_buf = [0u32; RMT_BUF_LEN];
    encode_sk6812(data, &mut rmt_buf);
    let tx = channel.transmit(&rmt_buf).unwrap();
    let ch = tx.wait().unwrap();
    delay.delay_us(RES_US);
    ch
}

/// 运行一个测试脚本 num_frames 帧，打印首尾两帧结果
fn run_test(ctx: &mut Context, name: &str, script: &str, num_frames: usize,
            channel: &mut Option<LedChannel>, delay: &mut Delay) {
    println!("[TEST] {} — 加载中...", name);
    ctx.reset_user_state();
    if let Err(e) = ctx.eval(script) {
        println!("[TEST] {} 编译失败: {}", name, e);
        return;
    }
    let call_update = match ctx.compile("update();") {
        Ok(bc) => bc,
        Err(e) => { println!("[TEST] {} compile update 失败: {}", name, e); return; }
    };
    println!("[TEST] {} — 运行 {} 帧", name, num_frames);
    for f in 0..num_frames {
        if let Err(e) = ctx.execute(&call_update) {
            println!("[TEST] {} frame={} 错误: {}", name, f, e);
            return;
        }
        let ch = channel.take().unwrap();
        *channel = Some(unsafe { send_leds(ch, delay, &LED_BUFFER) });
        if f == 0 || f == num_frames - 1 {
            print_led_frame(name, f);
        }
        delay.delay_ms(16u32);
    }
    println!("[TEST] {} — 通过", name);
}

fn run_js_tests(ctx: &mut Context, channel: &mut Option<LedChannel>, delay: &mut Delay) {
    println!("=== JS 灯效自检开始 ===");
    run_test(ctx, "纯红",   TEST_RED,     180, channel, delay);
    run_test(ctx, "追逐",   TEST_CHASE,   180, channel, delay);
    run_test(ctx, "呼吸",   TEST_BREATHE, 180, channel, delay);
    run_test(ctx, "彩虹",   TEST_RAINBOW, 180, channel, delay);
    println!("=== JS 灯效自检完成 ===");
}

// =============================================
// JS 引擎辅助函数
// =============================================

fn create_js_context() -> Context {
    let mut ctx = Context::new(64 * 1024);
    ctx.register_native("setLed", native_set_led as NativeFn, 4);
    ctx.register_native("getLedCount", native_get_led_count as NativeFn, 0);
    ctx.register_native("setAll", native_set_all as NativeFn, 3);
    ctx.eval(PRELUDE_JS).expect("prelude must compile");
    ctx
}

/// Reset user script state while preserving prelude functions.
/// Native functions survive reset_user_state(), but JS-defined functions do not,
/// so we must re-eval the prelude after every reset.
fn reload_prelude(ctx: &mut Context) {
    ctx.reset_user_state();
    ctx.eval(PRELUDE_JS).expect("prelude must compile");
}

fn compile_update_script(ctx: &mut Context, script: &str) -> Result<FunctionBytecode, String> {
    reload_prelude(ctx);
    ctx.eval(script).map_err(|e| format!("{}", e))?;
    ctx
        .compile("update();")
        .map_err(|e| format!("{}", e))
}

unsafe fn flush_ota_buffer(
    flash: &mut Option<FlashStorage>,
    flash_erased: &mut bool,
) -> Result<(), &'static str> {
    if OTA_BUF_LEN == 0 {
        return Ok(());
    }

    if !*flash_erased {
        println!("[OTA] Erasing OTA partition...");
        if flash.is_none() {
            *flash = Some(FlashStorage::new());
        }
        let flash_storage = flash.as_mut().unwrap();
        ota::erase_ota_partition(flash_storage, 0, ota::OTA_PARTITION_SIZE)?;
        *flash_erased = true;
    }

    if flash.is_none() {
        *flash = Some(FlashStorage::new());
    }

    let flash_storage = flash.as_mut().unwrap();
    let write_offset = OTA_STATE.received.saturating_sub(OTA_BUF_LEN as u32);
    ota::write_firmware(flash_storage, write_offset, &OTA_BUF[..OTA_BUF_LEN])?;
    println!(
        "[OTA] Wrote {}/{} bytes",
        OTA_STATE.received,
        OTA_STATE.total_size
    );
    OTA_BUF_LEN = 0;
    Ok(())
}

fn log_memory_stats(label: &str, ctx: &Context) {
    let stats = ctx.memory_stats();
    println!(
        "[JS] {} heap={} used={} free={} stack={} strs={} arrays={} objs={} closures={}",
        label,
        stats.heap_size,
        stats.used,
        stats.free,
        stats.stack_used,
        stats.runtime_strings,
        stats.arrays,
        stats.objects,
        stats.closures
    );
}

// =============================================
// 标准库（prelude.js，固件内置，不通过 BLE 传输）
// =============================================
const PRELUDE_JS: &str = include_str!("../../../../esp/ledjs/ledjs/lib/prelude.js");

// =============================================
// 默认 JS 灯效脚本（彩虹效果）
// =============================================
const DEFAULT_SCRIPT: &str = r#"
var frame = 0;
function update() {
    var count = getLedCount();
    var i = 0;
    while (i < count) {
        var hue = (frame * 2 + i * 32) % 768;
        var r = 0;
        var g = 0;
        var b = 0;
        if (hue < 256) {
            r = 255 - hue;
            g = hue;
        } else if (hue < 512) {
            var h = hue - 256;
            g = 255 - h;
            b = h;
        } else {
            var h = hue - 512;
            b = 255 - h;
            r = h;
        }
        setLed(i, r, g, b);
        i = i + 1;
    }
    frame = frame + 1;
}
"#;

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // 初始化堆分配器（mquickjs 32KB + prelude ~40KB + BLE ~20KB ≈ 100KB）
    esp_alloc::heap_allocator!(size: 192 * 1024);

    let mut delay = Delay::new();

    // 初始化 RMT 外设（80MHz，1 tick = 12.5ns）
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

    println!("SK6812 + JS + BLE 灯效引擎启动");

    // JS 灯效自检（wifi_init 之前，此时系统堆最干净）
    #[cfg(debug_assertions)]
    {
        let mut ctx = create_js_context();
        run_js_tests(&mut ctx, &mut channel, &mut delay);
        // ctx 在此处 drop，释放所有自检占用的堆内存
    }

    // 初始化 esp-wifi（BLE 需要）
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let wifi_init = wifi_init(
        timg0.timer0,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
    )
    .unwrap();

    // 初始化 JS 引擎并加载默认脚本
    let mut ctx = create_js_context();
    ctx.eval(DEFAULT_SCRIPT).expect("default script must compile");
    // 预编译 update() 调用，后续每帧用 execute() 复用，不产生 bytecode 累积
    let mut call_update: FunctionBytecode = ctx.compile("update();")
        .expect("compile update() failed");
    let mut gc_frame_counter: u32 = 0;
    let mut stats_frame_counter: u32 = 0;
    log_memory_stats("startup", &ctx);
    println!("默认 JS 脚本加载成功");

    println!("进入 BLE + 灯效渲染循环");

    let mut bluetooth = peripherals.BT;
    let now = || Instant::now().duration_since_epoch().as_millis();

    // 外层循环：每次 BLE 断开后重新广播
    loop {
        let connector = BleConnector::new(&wifi_init, &mut bluetooth);
        let hci = HciConnector::new(connector, now);
        let mut ble = Ble::new(&hci);

        // 初始化 BLE 广播
        if let Err(e) = ble.init() {
            println!("BLE init 失败: {:?}", e);
            loop {
                if let Err(e) = ctx.execute(&call_update) {
                    println!("JS 错误: {}", e);
                }
                let ch = channel.take().unwrap();
                channel = Some(unsafe { send_leds(ch, &mut delay, &LED_BUFFER) });
                delay.delay_ms(16u32);
            }
        }

        let _ = ble.cmd_set_le_advertising_parameters();
        let _ = ble.cmd_set_le_advertising_data(
            create_advertising_data(&[
                AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
                AdStructure::CompleteLocalName("UAVLED"),
            ])
            .unwrap(),
        );
        let _ = ble.cmd_set_le_advertise_enable(true);
        println!("BLE 广播已启动，设备名: UAVLED");

        // BLE GATT 回调：脚本/OTA 写入
        // 协议：
        //   - 0x01 前缀或无前缀: JS 脚本（兼容现有），以 \0 结尾
        //   - 0x02: OTA 数据包
        //   - 0x03: OTA 状态查询
        //   - 0x04: OTA 确认重启
        // Flash storage (lazy init, only used for OTA)
/*
        static mut FLASH: Option<FlashStorage> = None;
        static mut FLASH_ERASED: bool = false;

        let mut script_write = |_offset: usize, data: &[u8]| {
            if data.is_empty() {
                return;
            }

            let cmd = data[0];

            match cmd {
                ota::CMD_OTA_DATA if data.len() >= 12 => {
                    // OTA 数据包
                    unsafe {
                        let (ok, _resp) = OTA_STATE.handle_packet(data);
                        if ok {
                            // 提取 payload
                            let len = data[11] as usize;
                            let payload = &data[12..12 + len.min(data.len() - 12)];

                            // 累积到 OTA 缓冲区
                            for &b in payload {
                                if OTA_BUF_LEN < OTA_BUF_SIZE {
                                    OTA_BUF[OTA_BUF_LEN] = b;
                                    OTA_BUF_LEN += 1;
                                }
                            }

                            // Write to Flash when buffer is full
                            if OTA_BUF_LEN >= OTA_BUF_SIZE {
                                // Erase OTA partition on first write
                                if !FLASH_ERASED {
                                    println!("[OTA] Erasing OTA partition...");
                                    if FLASH.is_none() {
                                        FLASH = Some(FlashStorage::new());
                                    }
                                    let flash = FLASH.as_mut().unwrap();
                                    match ota::erase_ota_partition(flash, 0, ota::OTA_PARTITION_SIZE) {
                                        Ok(_) => FLASH_ERASED = true,
                                        Err(e) => {
                                            println!("[OTA] Erase failed: {}", e);
                                            OTA_STATE.abort();
                                            OTA_BUF_LEN = 0;
                                            return;
                                        }
                                    }
                                }

                                // Write Flash
                                if FLASH.is_none() {
                                    FLASH = Some(FlashStorage::new());
                                }
                                let flash = FLASH.as_mut().unwrap();
                                let write_offset = OTA_STATE.received - OTA_BUF_LEN as u32;
                                match ota::write_firmware(flash, write_offset, &OTA_BUF[..OTA_BUF_LEN]) {
                                    Ok(_) => println!("[OTA] Wrote {}/{} bytes", OTA_STATE.received, OTA_STATE.total_size),
                                    Err(e) => {
                                        println!("[OTA] Write failed: {}", e);
                                        OTA_STATE.abort();
                                    }
                                }
                                OTA_BUF_LEN = 0;
                            }

*/
        static mut FLASH: Option<FlashStorage> = None;
        static mut FLASH_ERASED: bool = false;
        static mut OTA_RESP_LEN: usize = 0;

        let readback_value = b"UAVLED JS";

        let mut script_read = |_offset: usize, data: &mut [u8]| {
            unsafe {
                let response = if OTA_RESP_LEN == 0 {
                    &readback_value[..]
                } else {
                    ota::OtaState::get_response(OTA_RESP_LEN)
                };
                let len = data.len().min(response.len());
                data[..len].copy_from_slice(&response[..len]);
                len
            }
        };

        let mut script_write = |_offset: usize, data: &[u8]| {
            if data.is_empty() {
                return;
            }

            let cmd = data[0];

            match cmd {
                ota::CMD_OTA_DATA | ota::CMD_OTA_STATUS | ota::CMD_OTA_COMMIT => unsafe {
                    let (ok, resp_len) = OTA_STATE.handle_packet(data);
                    OTA_RESP_LEN = resp_len;

                    if !ok {
                        return;
                    }

                    if cmd == ota::CMD_OTA_DATA && data.len() >= 12 {
                        let len = data[11] as usize;
                        let payload = &data[12..12 + len.min(data.len().saturating_sub(12))];

                        for &b in payload {
                            if OTA_BUF_LEN < OTA_BUF_SIZE {
                                OTA_BUF[OTA_BUF_LEN] = b;
                                OTA_BUF_LEN += 1;
                            }
                        }

                        if OTA_BUF_LEN >= OTA_BUF_SIZE {
                            if let Err(e) = flush_ota_buffer(&mut FLASH, &mut FLASH_ERASED) {
                                println!("[OTA] Write failed: {}", e);
                                OTA_STATE.abort();
                                OTA_BUF_LEN = 0;
                            }
                        }
                    }

                    if cmd == ota::CMD_OTA_COMMIT {
                        if let Err(e) = flush_ota_buffer(&mut FLASH, &mut FLASH_ERASED) {
                            println!("[OTA] Final write failed: {}", e);
                            OTA_STATE.abort();
                            OTA_BUF_LEN = 0;
                            return;
                        }
                        println!("[OTA] Commit accepted, rebooting");
                        software_reset();
                    }
                },
                _ => unsafe {
                    let payload = if cmd == ota::CMD_SCRIPT {
                        &data[1..]
                    } else {
                        data
                    };
                    let mut appended = 0usize;

                    for &b in payload {
                        if b == 0 {
                            SCRIPT_READY = true;
                            println!("BLE 脚本接收完成 ({} 字节)", SCRIPT_LEN);
                            break;
                        }

                        if SCRIPT_LEN >= SCRIPT_BUF_SIZE {
                            println!("BLE 脚本缓冲区已满，忽略剩余数据");
                            break;
                        }

                        SCRIPT_BUF[SCRIPT_LEN] = b;
                        SCRIPT_LEN += 1;
                        appended += 1;
                    }

                    OTA_RESP_LEN = 0;
                    println!("BLE 追加 {} 字节 (总计 {})", appended, SCRIPT_LEN);
                },
            }
        };

        gatt!([service {
            uuid: "a0e4f5e0-1234-4b56-9abc-def012345678",
            characteristics: [characteristic {
                name: "uavled_script",
                uuid: "a0e4f5e1-1234-4b56-9abc-def012345678",
                read: script_read,
                write: script_write,
            },],
        },]);

        let mut rng = bleps::no_rng::NoRng;
        let mut srv = AttributeServer::new(&mut ble, &mut gatt_attributes, &mut rng);
        let mut disconnect_after_apply = false;

        loop {
            if unsafe { SCRIPT_READY } {
                let script_len = unsafe { SCRIPT_LEN };
                println!("加载新 JS 脚本 ({} 字节)...", script_len);

                let script_result = unsafe { core::str::from_utf8(&SCRIPT_BUF[..SCRIPT_LEN]) };
                match script_result {
                    Ok(script) => match compile_update_script(&mut ctx, script) {
                        Ok(new_call_update) => {
                            call_update = new_call_update;
                            ctx.gc();
                            gc_frame_counter = 0;
                            stats_frame_counter = 0;
                            log_memory_stats("after-script-load", &ctx);
                            disconnect_after_apply = true;
                            println!("新脚本加载成功");
                        }
                        Err(e) => println!("新脚本加载失败: {}", e),
                    },
                    Err(e) => println!("脚本 UTF-8 解码失败: {:?}", e),
                }

                unsafe {
                    SCRIPT_LEN = 0;
                    SCRIPT_READY = false;
                }
            }

            match srv.do_work() {
                Ok(WorkResult::DidWork) => {}
                Ok(WorkResult::GotDisconnected) => {
                    println!("BLE 连接已断开，重新广播");
                    break;
                }
                Err(err) => println!("BLE attribute server error: {:?}", err),
            }

            if disconnect_after_apply {
                let _ = srv.disconnect(0x13);
                disconnect_after_apply = false;
            }

            if let Err(e) = ctx.execute(&call_update) {
                println!("JS 错误: {}", e);
            }
            gc_frame_counter = gc_frame_counter.wrapping_add(1);
            if gc_frame_counter >= 16 {
                ctx.gc();
                gc_frame_counter = 0;
            }
            stats_frame_counter = stats_frame_counter.wrapping_add(1);
            if stats_frame_counter >= 60 {
                log_memory_stats("steady-state", &ctx);
                stats_frame_counter = 0;
            }
            let ch = channel.take().unwrap();
            channel = Some(unsafe { send_leds(ch, &mut delay, &LED_BUFFER) });
            delay.delay_ms(16u32);
        }
    }
}
