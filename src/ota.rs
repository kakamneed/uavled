//! BLE OTA 模块 — 支持通过 BLE 无线更新固件
//!
//! 协议格式（复用特征值 a0e4f5e1-...）：
//! - 0x01: JS 脚本（兼容现有）
//! - 0x02: OTA 数据包
//! - 0x03: OTA 状态查询
//! - 0x04: OTA 确认重启
//!
//! OTA 数据包格式（最大 20 字节）：
//! [0x02][seq:u16_le][total:u32_le][offset:u32_le][len:u8][data:0-8B][checksum:u8]

use embedded_storage::nor_flash::NorFlash;
use esp_println::println;

/// OTA 分区配置（与 partitions-ota.csv 对应）
/// factory: 0x10000, 1.625MB
/// ota_0:   0x1B0000, 1.625MB
pub const OTA_PARTITION_OFFSET: u32 = 0x190000;
pub const OTA_PARTITION_SIZE: u32 = 0x180000; // 1.5MB

/// OTA 命令前缀
pub const CMD_SCRIPT: u8 = 0x01;
pub const CMD_OTA_DATA: u8 = 0x02;
pub const CMD_OTA_STATUS: u8 = 0x03;
pub const CMD_OTA_COMMIT: u8 = 0x04;

/// 响应数据（静态存储）
static mut OTA_RESPONSE: [u8; 8] = [0; 8];

/// OTA 状态
#[derive(Clone, Copy, Debug)]
pub struct OtaState {
    /// 固件总大小
    pub total_size: u32,
    /// 已接收字节数
    pub received: u32,
    /// 上一包序号（用于检测丢包）
    pub last_seq: u16,
    /// 是否正在 OTA
    pub in_progress: bool,
    /// Flash 写入偏移（按 sector 对齐）
    pub flash_offset: u32,
}

impl OtaState {
    pub const fn new() -> Self {
        Self {
            total_size: 0,
            received: 0,
            last_seq: 0xFFFF, // 初始为无效序号
            in_progress: false,
            flash_offset: 0,
        }
    }

    /// 开始 OTA 会话
    pub fn begin(&mut self, total_size: u32) -> Result<(), &'static str> {
        if total_size > OTA_PARTITION_SIZE {
            return Err("firmware too large");
        }
        self.total_size = total_size;
        self.received = 0;
        self.last_seq = 0xFFFF;
        self.in_progress = true;
        self.flash_offset = 0;
        println!("[OTA] 开始，总大小: {} 字节", total_size);
        Ok(())
    }

    /// 处理 OTA 数据包
    /// 返回 (是否成功, 响应数据长度)
    pub fn handle_packet(&mut self, data: &[u8]) -> (bool, usize) {
        if data.is_empty() {
            unsafe { OTA_RESPONSE[0] = 0x03; OTA_RESPONSE[1] = 0x00; }
            return (false, 2); // 错误：空数据
        }

        match data[0] {
            CMD_OTA_DATA if data.len() >= 12 => {
                // 解析头部
                let seq = u16::from_le_bytes([data[1], data[2]]);
                let total = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
                let offset = u32::from_le_bytes([data[7], data[8], data[9], data[10]]);
                let len = data[11] as usize;
                let payload = data.get(12..12 + len).unwrap_or(&[]);
                let checksum = *data.get(12 + len).unwrap_or(&0);

                // 校验 checksum
                let calc_checksum = payload.iter().fold(0u8, |a, b| a ^ b);
                if calc_checksum != checksum {
                    println!("[OTA] checksum 错误: 计算={}, 收到={}", calc_checksum, checksum);
                    unsafe { OTA_RESPONSE[0] = 0x03; OTA_RESPONSE[1] = 0x01; }
                    return (false, 2); // checksum 错误
                }

                // 首包初始化
                if !self.in_progress {
                    if let Err(_e) = self.begin(total) {
                        unsafe { OTA_RESPONSE[0] = 0x03; OTA_RESPONSE[1] = 0x02; }
                        return (false, 2);
                    }
                }

                // 检查序号连续性（支持断点续传：允许 seq=0 重新开始）
                if seq == 0 {
                    // 断点续传或重新开始
                    println!("[OTA] seq=0，断点续传或重新开始");
                    self.last_seq = 0xFFFF;
                    self.received = 0;
                } else if seq != self.last_seq.wrapping_add(1) {
                    println!("[OTA] seq 不连续: 期望={}, 收到={}", self.last_seq.wrapping_add(1), seq);
                    // 返回当前进度让发送方调整
                    unsafe {
                        OTA_RESPONSE[0] = 0x03;
                        OTA_RESPONSE[1] = (self.received & 0xFF) as u8;
                        OTA_RESPONSE[2] = ((self.received >> 8) & 0xFF) as u8;
                        OTA_RESPONSE[3] = ((self.received >> 16) & 0xFF) as u8;
                        OTA_RESPONSE[4] = ((self.received >> 24) & 0xFF) as u8;
                    }
                    return (false, 5);
                }

                // 检查偏移
                if offset != self.received {
                    println!("[OTA] offset 不匹配: 期望={}, 收到={}", self.received, offset);
                    unsafe {
                        OTA_RESPONSE[0] = 0x03;
                        OTA_RESPONSE[1] = (self.received & 0xFF) as u8;
                        OTA_RESPONSE[2] = ((self.received >> 8) & 0xFF) as u8;
                        OTA_RESPONSE[3] = ((self.received >> 16) & 0xFF) as u8;
                        OTA_RESPONSE[4] = ((self.received >> 24) & 0xFF) as u8;
                    }
                    return (false, 5);
                }

                // 写入 Flash（延迟到 main.rs 中实际执行）
                // 这里只更新状态
                self.last_seq = seq;
                self.received += payload.len() as u32;

                // 进度
                if self.received % 10240 < payload.len() as u32 {
                    println!("[OTA] 进度: {}/{} ({:.1}%)",
                        self.received, self.total_size,
                        (self.received as f64 / self.total_size as f64) * 100.0
                    );
                }

                unsafe { OTA_RESPONSE[0] = 0x03; OTA_RESPONSE[1] = 0xFF; }
                (true, 2) // 成功
            }

            CMD_OTA_STATUS => {
                // 返回当前 OTA 状态
                unsafe {
                    OTA_RESPONSE[0] = 0x03;
                    OTA_RESPONSE[1] = (self.received & 0xFF) as u8;
                    OTA_RESPONSE[2] = ((self.received >> 8) & 0xFF) as u8;
                    OTA_RESPONSE[3] = ((self.received >> 16) & 0xFF) as u8;
                    OTA_RESPONSE[4] = ((self.received >> 24) & 0xFF) as u8;
                }
                (true, 5)
            }

            CMD_OTA_COMMIT => {
                // 确认并重启
                if !self.in_progress {
                    unsafe { OTA_RESPONSE[0] = 0x03; OTA_RESPONSE[1] = 0x03; }
                    return (false, 2); // 无 OTA 会话
                }
                if self.received != self.total_size {
                    println!("[OTA] 未完成: {}/{}", self.received, self.total_size);
                    unsafe { OTA_RESPONSE[0] = 0x03; OTA_RESPONSE[1] = 0x04; }
                    return (false, 2); // 未完成
                }
                println!("[OTA] 完成，准备重启...");
                self.in_progress = false;
                unsafe { OTA_RESPONSE[0] = 0x03; OTA_RESPONSE[1] = 0xFE; }
                (true, 2) // 成功，即将重启
            }

            _ => {
                unsafe { OTA_RESPONSE[0] = 0x03; OTA_RESPONSE[1] = 0x00; }
                (false, 2) // 未知命令
            }
        }
    }

    /// 获取响应数据
    pub fn get_response(len: usize) -> &'static [u8] {
        unsafe { &OTA_RESPONSE[..len.min(OTA_RESPONSE.len())] }
    }

    /// 中止 OTA
    pub fn abort(&mut self) {
        self.in_progress = false;
        self.received = 0;
        self.total_size = 0;
        println!("[OTA] 已中止");
    }
}

/// 擦除 OTA 分区
/// 注意：这是一个耗时操作，每 sector 约 100ms
pub fn erase_ota_partition(flash: &mut esp_storage::FlashStorage, offset: u32, size: u32) -> Result<(), &'static str> {
    let sector_size = esp_storage::FlashStorage::SECTOR_SIZE;
    let sectors = (size + sector_size - 1) / sector_size;
    println!("[OTA] 擦除 {} 个 sector...", sectors);

    for i in 0..sectors {
        let sector_offset = OTA_PARTITION_OFFSET + offset + (i * sector_size);
        flash.erase(sector_offset, sector_offset + sector_size)
            .map_err(|_e| {
                println!("[OTA] 擦除 sector {} 失败", i);
                "erase failed"
            })?;

        // 进度报告
        if i % 8 == 0 {
            println!("[OTA] 擦除进度: {}/{} sectors", i, sectors);
        }
    }

    println!("[OTA] 擦除完成");
    Ok(())
}

/// 写入固件数据到 Flash
pub fn write_firmware(flash: &mut esp_storage::FlashStorage, offset: u32, data: &[u8]) -> Result<(), &'static str> {
    let flash_offset = OTA_PARTITION_OFFSET + offset;
    flash.write(flash_offset, data)
        .map_err(|_e| {
            println!("[OTA] 写入失败");
            "write failed"
        })?;
    Ok(())
}
