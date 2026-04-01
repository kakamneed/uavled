# UAVLED

ESP32-C3 LED 灯效控制器，支持通过 BLE 动态下发 JavaScript 灯效脚本和 OTA 无线固件升级。

## 功能

- SK6812 LED 灯带驱动（GRB 格式，GPIO10）
- 内置 mquickjs JavaScript 引擎（no_std 移植版）
- BLE GATT 服务，支持实时下发 JS 脚本切换灯效
- **BLE OTA 无线固件升级**（通过 ledjs 工具）
- 默认彩虹灯效（断电重启后自动恢复）

## 硬件

| 引脚 | 功能 |
|------|------|
| GPIO10 | SK6812 数据线 |

## 构建与烧录

```bash
cargo build --release
espflash flash --monitor target/riscv32imc-unknown-none-elf/release/uavled
```

---

## BLE OTA 无线烧录

### 准备工作

1. 确保已安装 [ledjs](https://github.com/...) 工具
2. 准备固件文件 `firmware.bin`
3. 设备已通电并处于 BLE 广播状态

### 执行 OTA

```bash
ledjs ble-ota firmware.bin
```

**说明：**
- 设备会自动扫描并连接名为 `UAVLED` 的设备
- 如需指定设备地址：`ledjs ble-ota firmware.bin --device XX:XX:XX:XX:XX:XX`
- 烧录时间：约 2-3 分钟（500KB 固件）
- 烧录完成后设备会自动重启

### OTA 原理

| 阶段 | 说明 |
|------|------|
| 连接 | 扫描并连接 UAVLED 设备 |
| 握手 | 查询当前 OTA 状态（支持断点续传） |
| 传输 | 分批发送固件数据（每包约 7 字节） |
| 确认 | 每 50 包进行一次状态确认 |
| 提交 | 发送 COMMIT 命令，设备校验后重启 |

### 注意事项

- OTA 过程中请保持设备供电稳定
- 如 OTA 失败，可通过 USB 重新烧录恢复
- OTA 固件会写入到 OTA 分区（0x190000），不影响 factory 分区

---

## BLE 脚本下发

### 协议说明

设备广播名：`UAVLED`

| 属性 | 值 |
|------|----|
| Service UUID | `a0e4f5e0-1234-4b56-9abc-def012345678` |
| Characteristic UUID | `a0e4f5e1-1234-4b56-9abc-def012345678` |
| Properties | READ, WRITE |

**传输协议**：向 Characteristic 写入 JS 脚本内容，**末尾必须以 `\0`（0x00）字节结尾**作为传输完成标志。支持分多次写入（分包追加），收到 `\0` 后触发加载。

**为什么需要 `\0` 结尾？**

BLE GATT Write 操作受 MTU（最大传输单元）限制，单次写入最多只能传输约 18～125 字节（取决于 MTU 协商结果）。对于较长的 JS 脚本，客户端需要分多次写入。固件无法自动判断何时接收完毕，因此约定以 `\0` 字节作为传输结束的显式标记，收到后才触发脚本编译和加载。

---

### 方法一：nRF Connect（手机版）

适合发送短脚本（推荐测试用）。

**操作步骤：**

1. 打开 nRF Connect → SCANNER → 找到 `UAVLED` → CONNECT
2. 进入 CLIENT 标签，展开服务，找到 Characteristic（READ, WRITE）
3. 点击右侧 **↑ 上传箭头**（Write 按钮）
4. 在写入对话框中**分两步操作**：

**第一步：发送脚本内容**
- 格式选 TEXT
- 粘贴脚本内容（每次最多约 18 字节，需多次发送）
- 点 SEND

**第二步：发送结束标记**
- 格式切换为 BYTE ARRAY
- 输入 `00`
- 点 SEND

**注意**：nRF Connect 手机版不支持 Long Write，每次最多发送约 18 字节（MTU 23 - 3 字节 ATT 头 - 2 字节句柄）。发送较长脚本时需多次点 SEND 追加内容，最后单独发送 `00` 触发加载。

---

### 方法二：Python bleak（推荐，支持任意长度脚本）

适合从电脑发送完整脚本，无 MTU 限制，自动分包。

**安装依赖：**
```bash
pip install bleak
```

**发送脚本：**
```python
import asyncio
from bleak import BleakClient

ADDR = "DC:06:75:F7:70:18"  # 替换为你设备的 MAC 地址
CHR  = "a0e4f5e1-1234-4b56-9abc-def012345678"

script = """
var f = 0;
function update() {
    var i = 0;
    while (i < getLedCount()) {
        setLed(i, 0, 255, 0);
        i = i + 1;
    }
    f = f + 1;
}
"""

async def main():
    async with BleakClient(ADDR) as client:
        data = script.encode() + b'\x00'
        chunk = 18  # 每包字节数，可根据实际 MTU 调整
        for i in range(0, len(data), chunk):
            await client.write_gatt_char(CHR, data[i:i+chunk], response=True)
            print(f"已发送 {min(i+chunk, len(data))}/{len(data)} 字节")
        print("脚本发送完成")

asyncio.run(main())
```

设备 MAC 地址可在串口日志或 nRF Connect 连接界面查看。

---

### 方法三：Web Bluetooth（Chrome/Edge 手机版）

将以下内容保存为 `uavled.html`，用 Chrome 手机版打开：

```html
<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>UAVLED</title></head>
<body>
<h2>UAVLED 脚本发送</h2>
<textarea id="s" rows="12" cols="40">var f=0;
function update(){
  var i=0;
  while(i&lt;getLedCount()){
    setLed(i,0,255,0);
    i=i+1;
  }
  f=f+1;
}</textarea><br>
<button onclick="send()">连接并发送</button>
<div id="log"></div>
<script>
const SVC='a0e4f5e0-1234-4b56-9abc-def012345678';
const CHR='a0e4f5e1-1234-4b56-9abc-def012345678';
function log(m){document.getElementById('log').innerHTML+=m+'<br>';}
async function send(){
  const dev=await navigator.bluetooth.requestDevice({filters:[{name:'UAVLED'}],optionalServices:[SVC]});
  const srv=await(await dev.gatt.connect()).getPrimaryService(SVC);
  const chr=await srv.getCharacteristic(CHR);
  const data=new TextEncoder().encode(document.getElementById('s').value+'\0');
  for(let i=0;i<data.length;i+=18){
    await chr.writeValueWithResponse(data.slice(i,i+18));
    log('发送 '+Math.min(i+18,data.length)+'/'+data.length+' 字节');
  }
  log('完成！');
}
</script>
</body>
</html>
```

---

## JS 脚本编写规范

### 必须实现的函数

```javascript
function update() {
    // 每帧调用一次（约 60fps）
}
```

### 可用的原生函数

| 函数 | 参数 | 说明 |
|------|------|------|
| `setLed(i, r, g, b)` | index, red, green, blue (0-255) | 设置单个 LED 颜色 |
| `getLedCount()` | 无 | 返回 LED 数量（当前为 8） |
| `setAll(r, g, b)` | red, green, blue (0-255) | 设置所有 LED 为同一颜色 |

### 语法限制（mquickjs 不支持）

| 不支持 | 替代写法 |
|--------|----------|
| `i++` / `i--` | `i = i + 1` / `i = i - 1` |
| `+=` `-=` `*=` 等 | `x = x + 1` 展开形式 |
| `for (var i=0; i<n; i++)` | 改用 `while` 循环 |
| 箭头函数 `=>` | 普通 `function` |
| 模板字符串 `` ` `` | 字符串拼接 `+` |
| 语句末尾省略分号 | 必须加 `;` |

### 示例脚本

**1. 纯色常亮（全红）：**
```javascript
function update() {
    setAll(255, 0, 0);
}
```

**2. 呼吸灯（白色渐亮渐暗）：**
```javascript
var frame = 0;
function update() {
    var t = frame % 120;
    var b = t < 60 ? t * 4 : (120 - t) * 4;
    setAll(b, b, b);
    frame = frame + 1;
}
```

**3. 跑马灯（青色亮点流动）：**
```javascript
var pos = 0;
var frame = 0;
function update() {
    var count = getLedCount();
    var i = 0;
    while (i < count) {
        setLed(i, 0, 0, 0);
        i = i + 1;
    }
    setLed(pos, 0, 200, 255);
    frame = frame + 1;
    if (frame % 4 == 0) {
        pos = (pos + 1) % count;
    }
}
```

**4. 彩虹流动：**
```javascript
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
```

**5. 警报闪烁（红蓝交替）：**
```javascript
var frame = 0;
function update() {
    var phase = (frame / 15) % 4;
    if (phase < 1) {
        setAll(255, 0, 0);
    } else if (phase < 2) {
        setAll(0, 0, 0);
    } else if (phase < 3) {
        setAll(0, 0, 255);
    } else {
        setAll(0, 0, 0);
    }
    frame = frame + 1;
}
```

**6. 火焰效果（随机红橙跳动）：**
```javascript
var seeds = [13, 37, 71, 97, 53, 29, 83, 61];
function rand(idx) {
    seeds[idx] = (seeds[idx] * 1664525 + 1013904223) % 65536;
    return seeds[idx];
}
var frame = 0;
function update() {
    var count = getLedCount();
    var i = 0;
    while (i < count) {
        var r = 200 + rand(i) % 55;
        var g = rand(i) % 80;
        setLed(i, r, g, 0);
        i = i + 1;
    }
    frame = frame + 1;
}
```

**7. 流光渐变（青色尾迹）：**
```javascript
var frame = 0;
function update() {
    var count = getLedCount();
    var offset = frame % count;
    var i = 0;
    while (i < count) {
        var dist = (i - offset + count) % count;
        var b = 255 - dist * 32;
        if (b < 0) { b = 0; }
        setLed(i, 0, b, b);
        i = i + 1;
    }
    frame = frame + 1;
}
```

**8. 双向扫描（来回扫光）：**
```javascript
var frame = 0;
function update() {
    var count = getLedCount();
    var period = count * 2 - 2;
    var t = frame % period;
    var pos = t < count ? t : period - t;
    var i = 0;
    while (i < count) {
        var dist = i - pos;
        if (dist < 0) { dist = -dist; }
        var b = 200 - dist * 60;
        if (b < 0) { b = 0; }
        setLed(i, b, 0, b);
        i = i + 1;
    }
    frame = frame + 1;
}
```

**9. 交替颜色闪烁（奇偶灯交替）：**
```javascript
var frame = 0;
function update() {
    var count = getLedCount();
    var phase = (frame / 20) % 2;
    var i = 0;
    while (i < count) {
        if ((i % 2 == 0) == (phase < 1)) {
            setLed(i, 255, 100, 0);
        } else {
            setLed(i, 0, 50, 255);
        }
        i = i + 1;
    }
    frame = frame + 1;
}
```

**10. 关灯（全灭）：**
```javascript
function update() {
    setAll(0, 0, 0);
}
```

---

## 串口日志说明

正常启动输出：
```
SK6812 + JS + BLE 灯效引擎启动
默认 JS 脚本加载成功
进入 BLE + 灯效渲染循环
BLE 广播已启动，设备名: UAVLED
```

收到脚本时输出：
```
BLE 追加 18 字节 (总计 18)
BLE 追加 18 字节 (总计 36)
...
BLE 脚本接收完成 (xxx 字节)
加载新 JS 脚本 (xxx 字节)...
新脚本加载成功
```
