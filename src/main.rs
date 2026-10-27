//! 红魔散热器 8 Pro 命令行控制器
//!
//! 目标设备：RM Magcooler 8pro / PA1256（红魔磁吸散热器 8 Pro）
//! 协议来源：官方 App `cn.nubia.externdevice` 的 smali 反编译 + 社区实测抓包
//!
//! 设计原则：一次调用 = 连接 → 写一两条 → 断开 → 退出。
//! 不做常驻进程、不占后台、不留连接（BLE 同时只允许一个中心）。

use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Manager, Peripheral};
use futures::StreamExt;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use std::process::Command;
use std::time::{Duration, Instant};
use tokio::time::sleep;

type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 官方主服务，所有 0x101x 特征都挂在这下面
const SVC: &str = "d52082ad-e805-9f97-9d4e-1c682d9c9ce6";

const CH_HALL: &str = "00001011-0000-1000-8000-00805f9b34fb"; // 制冷总开关 02 开 / 03 关
const CH_FAN: &str = "00001012-0000-1000-8000-00805f9b34fb"; // 风扇 raw 40..80
const CH_LIGHT: &str = "00001013-0000-1000-8000-00805f9b34fb"; // [mode][R][G][B]
const CH_TEMP: &str = "00001014-0000-1000-8000-00805f9b34fb"; // 温度 notify
const CH_STATUS: &str = "00001015-0000-1000-8000-00805f9b34fb"; // 状态包 notify
const CH_DIABLO: &str = "00001017-0000-1000-8000-00805f9b34fb"; // 破坏神 01/00
const CH_AUTO: &str = "00001018-0000-1000-8000-00805f9b34fb"; // 智能温控 01/00

/// 温度偏移：开源逆向与抓包双重确认，显示值 = 原始字节 - 6
/// （样例 `04 08 00 00` → 2°C，`04 03 00 00` → -3°C）
const TEMP_OFFSET: i8 = 6;

/// 8 Pro 的 8 个挡位对应的 raw 值（实测表，非线性）
const FAN_LEVELS: [u8; 8] = [0x2e, 0x34, 0x3a, 0x40, 0x44, 0x48, 0x4c, 0x50];

const HELP: &str = "\
rmcooler —— 红魔散热器 8 Pro · 状态栏控制

不带参数直接运行 → 状态栏模式（菜单栏常驻，温度档位可见，点开即操作）

快捷命令:
  hm off                          关制冷
  hm on                           开制冷 + 6 档
  hm status                       读一次状态
  hm report [YYYY-MM-DD]          温度/功耗/档位分布摘要
  hm scan                         扫描附近的散热器
  hm raw [--for 15]               原始帧抓包（校准用）
  hm start 6 / stop / level 5     档位与制冷开关（.app 双击用）
  hm boost on|off / auto on|off   破坏神 / 设备温控

后台服务:
  hm service install              开机自启（后台自动调档，无界面）
  hm service stop                 停后台服务（想开状态栏前先执行）
  hm service uninstall            卸载后台服务

自动调档:
  hm daemon [--idle-min 5]        后台自动调档（状态栏不开时的替代）

注：状态栏运行时其他命令会被单实例锁挡住，请用状态栏菜单操作。
";

#[tokio::main]
async fn main() -> R<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 面板/纯文本/自动巡航都会独占蓝牙，先拿单实例锁
    let needs_lock = args.is_empty()
        || matches!(
            args[0].as_str(),
            "menubar" | "mb" | "daemon" | "autopilot" | "auto-pilot"
        );
    let lock_mode = if args.is_empty() {
        "menubar"
    } else {
        match args[0].as_str() {
            "menubar" | "mb" => "menubar",
            "daemon" | "autopilot" | "auto-pilot" => "daemon",
            _ => "cli",
        }
    };
    let _lock = if needs_lock {
        match acquire_lock(lock_mode) {
            Ok(_) => Some(LockGuard),
            Err(e) => {
                eprintln!("✗ {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    // 不带参数：后台化启动状态栏（脱离终端，窗口随手关）
    if args.is_empty() {
        let exe = std::env::current_exe().map_err(|e| format!("找不到自身: {e}"))?;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let child = {
            let mut c = Command::new(exe);
            c.arg("menubar")
                .arg("--detached")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            c.process_group(0); // 脱离终端进程组，关窗口不收 SIGHUP
            c.spawn()
        }
        .map_err(|e| format!("启动失败: {e}"))?;
        println!("✓ 状态栏已启动（PID {}），看屏幕右上角；这个终端可以关掉", child.id());
        return Ok(());
    }

    if args[0] == "-h" || args[0] == "--help" || args[0] == "help" {
        println!("{HELP}");
        return Ok(());
    }

    run(&args).await.map_err(|e| {
        eprintln!("✗ {e}");
        std::process::exit(1);
        #[allow(unreachable_code)]
        e
    })?;
    Ok(())
}

async fn run(args: &[String]) -> R<()> {
    let cmd = args[0].as_str();

    // scan 不需要连接
    if cmd == "scan" {
        return scan_only().await;
    }

    // autopilot 自己管连接与重连
    if cmd == "autopilot" || cmd == "auto-pilot" {
        return autopilot(args).await;
    }

    // 纯文本滚动模式：不切全屏、不用颜色，任何终端都能看

    // 全屏实时面板（裸跑 rmcooler 也是这个）

    // 状态栏模式（--detached 由无参数入口后台拉起）
    if cmd == "menubar" || cmd == "mb" {
        let detached = args.iter().any(|a| a == "--detached");
        return menubar_main(detached).await;
    }

    // 快速开关（不开界面；状态栏运行时会被单实例锁挡住，请用菜单）
    if cmd == "off" || cmd == "on" {
        let p = open().await?;
        if cmd == "off" {
            write(&p, CH_HALL, &[0x03]).await?;
            println!("✓ 制冷已关");
        } else {
            write(&p, CH_HALL, &[0x02]).await?;
            write(&p, CH_FAN, &[FAN_LEVELS[5]]).await?;
            println!("✓ 制冷已开，第 6 档");
        }
        return Ok(());
    }

    // 档位/开关子命令（RedMagicCooler.app 与脚本使用）
    if cmd == "stop" {
        let p = open().await?;
        write(&p, CH_HALL, &[0x03]).await?;
        println!("✓ 制冷已关");
        return Ok(());
    }
    if cmd == "start" || cmd == "level" {
        let p = open().await?;
        let level: u8 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(6);
        if !(1..=8).contains(&level) {
            return Err("档位 1-8".into());
        }
        write(&p, CH_HALL, &[0x02]).await?;
        write(&p, CH_FAN, &[FAN_LEVELS[(level - 1) as usize]]).await?;
        println!("✓ 制冷开，第 {level} 档");
        return Ok(());
    }
    if cmd == "boost" || cmd == "auto" {
        let p = open().await?;
        let on = args.get(1).map(|s| s == "on").unwrap_or(true);
        let (uuid, name) = if cmd == "boost" {
            (CH_DIABLO, "破坏神")
        } else {
            (CH_AUTO, "智能温控")
        };
        write(&p, uuid, &[if on { 0x01 } else { 0x00 }]).await?;
        println!("✓ {name} {}", if on { "开" } else { "关" });
        return Ok(());
    }

    // 后台自动调档（service install 的本体）
    if cmd == "daemon" {
        return autopilot(&args[1..]).await;
    }

    // 数据摘要
    if cmd == "report" {
        let date = args.get(1).cloned().unwrap_or_else(today_str);
        return report(&date);
    }

    // 后台服务管理
    if cmd == "service" {
        return match args.get(1).map(|x| x.as_str()) {
            Some("install") => service_install(),
            Some("stop") => service_stop(),
            Some("uninstall") => service_uninstall(),
            other => Err(format!(
                "用法: rmcooler service install|stop|uninstall，收到 {other:?}"
            )
            .into()),
        };
    }

    // 原始帧抓包（校准用）
    if cmd == "raw" {
        return raw_watch(args).await;
    }

    // sensors 只读本机状态，不碰蓝牙
    if cmd == "sensors" {
        let heat = thermal_state();
        let idle = idle_seconds();
        println!("热压力    {}（{}）", heat_name(heat), heat);
        println!("锁屏      {}", if screen_locked() { "是" } else { "否" });
        println!("空闲      {} 秒", idle);
        println!(
            "→ 目标档位 {}",
            level_for_heat(
                heat,
                args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2),
                args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8)
            )
        );
        return Ok(());
    }

    let p = open().await?;
    let result = dispatch(&p, args).await;
    // 无论成败都干净断开，不给下一次调用留占用
    let _ = p.disconnect().await;
    result
}

async fn dispatch(p: &Peripheral, args: &[String]) -> R<()> {
    let cmd = args[0].as_str();
    let arg = |i: usize| -> R<&str> {
        args.get(i)
            .map(|s| s.as_str())
            .ok_or_else(|| format!("`{cmd}` 缺少参数").into())
    };
    let onoff = |s: &str| -> R<bool> {
        match s {
            "on" | "1" | "true" | "开" => Ok(true),
            "off" | "0" | "false" | "关" => Ok(false),
            other => Err(format!("只接受 on / off，收到 `{other}`").into()),
        }
    };

    match cmd {
        "power" | "on" | "off" => {
            let want = if cmd == "on" {
                true
            } else if cmd == "off" {
                false
            } else {
                onoff(arg(1)?)?
            };
            write(p, CH_HALL, &[if want { 0x02 } else { 0x03 }]).await?;
            println!("{} 制冷总开关", if want { "✓ 开" } else { "✓ 关" });
        }

        "level" | "fan" => {
            let n: u8 = arg(1)?.parse().map_err(|_| "挡位必须是 1-8 的整数")?;
            if !(1..=8).contains(&n) {
                return Err("挡位范围是 1-8".into());
            }
            let raw = FAN_LEVELS[(n - 1) as usize];
            write(p, CH_FAN, &[raw]).await?;
            println!("✓ 第 {n} 挡（raw {raw} / 0x{raw:02x}）");
        }

        "boost" | "diablo" => {
            let want = onoff(arg(1)?)?;
            if want {
                // 官方逻辑：开破坏神前先关智能温控
                write(p, CH_AUTO, &[0x00]).await?;
                write(p, CH_DIABLO, &[0x01]).await?;
                println!("✓ 破坏神 开（已自动关闭智能温控）");
            } else {
                write(p, CH_DIABLO, &[0x00]).await?;
                println!("✓ 破坏神 关");
            }
        }

        "auto" => {
            let want = onoff(arg(1)?)?;
            if want {
                write(p, CH_DIABLO, &[0x00]).await?;
                write(p, CH_AUTO, &[0x01]).await?;
                println!("✓ 智能温控 开（已自动关闭破坏神）");
            } else {
                write(p, CH_AUTO, &[0x00]).await?;
                println!("✓ 智能温控 关");
            }
        }

        "light" => {
            let mode = arg(1)?;
            let payload: [u8; 4] = match mode {
                "colorful" | "rainbow" => [0x01, 0, 0, 0],
                "breath" | "breathe" => [0x02, 0, 0, 0],
                "off" | "close" => [0x06, 0, 0, 0],
                "fixed" | "on" => {
                    let hex = args.get(2).map(|s| s.as_str()).unwrap_or("#ff0000");
                    let (r, g, b) = parse_color(hex)?;
                    [0x04, r, g, b]
                }
                other => return Err(format!("未知灯效 `{other}`（colorful/breath/fixed/off）").into()),
            };
            write(p, CH_LIGHT, &payload).await?;
            println!("✓ 灯效 {mode}  [{:02x} {:02x} {:02x} {:02x}]", payload[0], payload[1], payload[2], payload[3]);
        }

        "status" => {
            let watch = args.iter().any(|a| a == "-w" || a == "--watch");
            print_status(p, watch).await?;
        }

        "start" => {
            let n: u8 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5);
            if !(1..=8).contains(&n) {
                return Err("挡位范围是 1-8".into());
            }
            write(p, CH_HALL, &[0x02]).await?;
            write(p, CH_FAN, &[FAN_LEVELS[(n - 1) as usize]]).await?;
            println!("✓ 已开启制冷，第 {n} 挡");
        }

        "stop" => {
            write(p, CH_HALL, &[0x03]).await?;
            println!("✓ 已关闭制冷");
        }

        other => return Err(format!("未知命令 `{other}`，用 --help 看用法").into()),
    }
    Ok(())
}

/* ---------------- BLE ---------------- */

fn name_matches(name: &str) -> bool {
    if let Ok(custom) = std::env::var("RMCOOLER_NAME") {
        if !custom.is_empty() {
            return name.to_ascii_lowercase().contains(&custom.to_ascii_lowercase());
        }
    }
    let n = name.to_ascii_lowercase();
    n.contains("magcooler") || n.contains("redmagic") || n.contains("rm ")
}

async fn scan_only() -> R<()> {
    let v = std::env::var("RMCOOLER_VERBOSE").is_ok();
    if v { eprintln!("[ble] step1 Manager::new()"); }
    let manager = Manager::new().await?;
    if v { eprintln!("[ble] step2 adapters()"); }
    let central = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or("没有可用的蓝牙适配器")?;
    if v { eprintln!("[ble] step3 start_scan()"); }

    central.start_scan(ScanFilter::default()).await?;
    println!("扫描 6 秒…（官方 App 连着时设备不会广播）");
    sleep(Duration::from_secs(6)).await;

    let mut found = 0;
    for p in central.peripherals().await? {
        if let Some(props) = p.properties().await? {
            let name = props.local_name.unwrap_or_default();
            if name_matches(&name) {
                let rssi = props.rssi.map(|r| format!("{r} dBm")).unwrap_or_else(|| "-".into());
                println!("  ✓ {name}   {rssi}   {}", p.address());
                found += 1;
            }
        }
    }
    central.stop_scan().await?;

    if found == 0 {
        println!("  没有匹配到散热器。");
        println!("  · 确认已通电（霍尔/按键触发后才会广播）");
        println!("  · 先在手机上杀掉红魔装备库：adb shell am force-stop cn.nubia.externdevice");
        println!("  · 或用 RMCOOLER_NAME=RM 再扫一次");
    }
    Ok(())
}

/// TUI 占屏期间必须静默：任何 println 都会把全屏界面打花


fn step(msg: &str) {
    use std::io::Write;
    println!("  · {msg}");
    let _ = std::io::stdout().flush();
}

/// 连接设备。每一步都有进度输出和超时，卡住时必须能说出卡在哪。
async fn open() -> R<Peripheral> {
    step("初始化蓝牙（若系统弹出权限请求，请点「允许」）");
    let manager = match tokio::time::timeout(Duration::from_secs(15), Manager::new()).await {
        Ok(r) => r?,
        Err(_) => {
            return Err("蓝牙初始化超时。\n\
                 多半是没拿到授权：打开「系统设置 → 隐私与安全性 → 蓝牙」，\n\
                 把你现在用的终端（Terminal 或 iTerm）勾上，然后关掉这个窗口重开。"
                .into())
        }
    };

    step("获取蓝牙适配器");
    let adapters = match tokio::time::timeout(Duration::from_secs(15), manager.adapters()).await {
        Ok(r) => r?,
        Err(_) => return Err("获取蓝牙适配器超时（蓝牙可能被系统挂起，先关一次再开 Mac 蓝牙）".into()),
    };
    let central = adapters
        .into_iter()
        .next()
        .ok_or("系统报告没有可用的蓝牙适配器：确认 Mac 菜单栏的蓝牙已打开")?;

    let scan_secs: u64 = std::env::var("RMCOOLER_SCAN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    step(&format!("扫描附近设备（最多 {scan_secs} 秒，插上电源会自动出现）"));
    central.start_scan(ScanFilter::default()).await?;

    let deadline = Instant::now() + Duration::from_secs(scan_secs);
    let mut target: Option<Peripheral> = None;
    let mut seen: Vec<String> = Vec::new();

    while Instant::now() < deadline {
        let list = match tokio::time::timeout(Duration::from_secs(3), central.peripherals()).await {
            Ok(Ok(v)) => v,
            _ => continue,
        };
        for p in list {
            if let Ok(Ok(Some(props))) =
                tokio::time::timeout(Duration::from_secs(2), p.properties()).await
            {
                let name = props.local_name.unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                if !seen.iter().any(|x| x == &name) {
                    seen.push(name.clone());
                    if name_matches(&name) {
                        step(&format!("✓ 命中 {name}"));
                    } else {
                        step(&format!("+ {name}"));
                    }
                }
                if name_matches(&name) {
                    target = Some(p);
                    break;
                }
            }
        }
        if target.is_some() {
            break;
        }
        sleep(Duration::from_millis(400)).await;
    }
    let _ = central.stop_scan().await;

    let p = match target {
        Some(p) => p,
        None => {
            let mut msg = String::new();
            if seen.is_empty() {
                msg.push_str("没扫描到任何蓝牙设备。逐项确认：\n");
                msg.push_str("  1. 散热器通电了吗？（USB-C 接 30W 以上电源，插上应有灯）\n");
                msg.push_str("  2. 手机上的「红魔装备库」是不是还连着？它连着时设备不对外广播\n");
                msg.push_str("  3. Mac 菜单栏的蓝牙打开了吗？\n");
                msg.push_str("  4. 终端有没有蓝牙权限？（系统设置 → 隐私与安全性 → 蓝牙）\n");
                msg.push_str("插上电源后重新运行本命令即可，扫描中会逐条列出发现的设备。");
            } else {
                msg.push_str(&format!("没匹配到散热器，但扫到了 {} 个设备：\n", seen.len()));
                for n in seen.iter().take(12) {
                    msg.push_str(&format!("  - {n}\n"));
                }
                msg.push_str("若上面有你的散热器，用环境变量指定关键字：\n");
                msg.push_str("  RMCOOLER_NAME=magcooler ./target/release/rmcooler");
            }
            return Err(msg.into());
        }
    };

    step(&format!("找到设备 {}，正在连接", p.address()));

    // 设备断电重上电后，macOS 会保留一段 stale 连接状态：
    // is_connected 误报 true → 跳过 connect → 后面全部失败，重试多少次都一样。
    // 所以每次都先强制断开一次，保证接下来协商的是全新链路。
    let _ = tokio::time::timeout(Duration::from_secs(3), p.disconnect()).await;
    sleep(Duration::from_millis(500)).await;

    let mut connected = false;
    for attempt in 1..=2u8 {
        step(&format!("连接（第 {attempt} 次）"));
        match tokio::time::timeout(Duration::from_secs(15), p.connect()).await {
            Ok(Ok(())) => {
                connected = true;
                break;
            }
            Ok(Err(e)) => step(&format!("连接失败: {e}")),
            Err(_) => step("连接超时"),
        }
        // 失败后清掉可能的 stale 链路再重试
        let _ = tokio::time::timeout(Duration::from_secs(3), p.disconnect()).await;
        sleep(Duration::from_secs(1)).await;
    }
    if !connected {
        return Err("连接失败（重试 2 次）。设备若刚断电重上电，等 5 秒再点一次重连".into());
    }

    step("读取服务与特征");
    match tokio::time::timeout(Duration::from_secs(10), p.discover_services()).await {
        Ok(r) => r?,
        Err(_) => return Err("读取服务超时".into()),
    }
    sleep(Duration::from_millis(250)).await;

    let n = p.characteristics().len();
    if n == 0 {
        return Err("连上了但读不到任何特征，设备可能不是 8 Pro".into());
    }
    let has_svc = p
        .services()
        .iter()
        .any(|s| s.uuid.to_string().eq_ignore_ascii_case(SVC));
    if !has_svc {
        step("注意：没发现红魔主服务，设备可能不是 8 Pro");
    }
    step(&format!("连接就绪，共 {n} 个特征"));

    Ok(p)
}

fn find(p: &Peripheral, uuid: &str) -> R<btleplug::api::Characteristic> {
    p.characteristics()
        .into_iter()
        .find(|c| c.uuid.to_string().eq_ignore_ascii_case(uuid))
        .ok_or_else(|| format!("设备上找不到特征 {uuid}（型号可能不是 8 Pro）").into())
}

async fn write(p: &Peripheral, uuid: &str, data: &[u8]) -> R<()> {
    let ch = find(p, uuid)?;

    // 设备有个别特征响应很慢（实测 1014 订阅会超时）。写入必须设上限，
    // 否则一次卡住就会把整个界面冻结，看起来像「按键没反应」。
    let with_resp = p.write(&ch, data, WriteType::WithResponse);
    match tokio::time::timeout(Duration::from_secs(3), with_resp).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) | Err(_) => {
            // 有响应写失败/超时 → 退化为无响应写，只发不等
            let no_resp = p.write(&ch, data, WriteType::WithoutResponse);
            tokio::time::timeout(Duration::from_secs(2), no_resp)
                .await
                .map_err(|_| "写入超时（设备无响应）")??;
        }
    }

    // 留一点时间让固件处理，否则立刻断开容易丢包
    sleep(Duration::from_millis(120)).await;
    Ok(())
}

/// 状态包解析：[tag, b0, b1, b2]
fn decode_status(frame: &[u8], temp_offset: i8) -> Option<String> {
    if frame.is_empty() {
        return None;
    }
    let tag = frame[0];
    let b0 = *frame.get(1).unwrap_or(&0);
    let b1 = *frame.get(2).unwrap_or(&0);

    Some(match tag {
        0x04 => format!("温度 {}°C", (b0 as i8).saturating_sub(temp_offset)),
        0x05 => format!("负载/保护 [{}]", hex(frame)),
        0x07 => {
            if b0 < 8 {
                format!("供电限档 {b0}/8 ⚠ 供电不足")
            } else {
                "供电限档 不限".to_string()
            }
        }
        0x08 => format!("转速 {} rpm", ((b0 as u16) << 8) | b1 as u16),
        0x09 => format!("功率 {b0} W"),
        _ => format!("tag {tag:02x} [{}]", hex(frame)),
    })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect::<Vec<_>>().join(" ")
}

async fn print_status(p: &Peripheral, watch: bool) -> R<()> {
    // 两个通知通道都订上
    for uuid in [CH_STATUS, CH_TEMP] {
        if let Ok(ch) = find(p, uuid) {
            let _ = p.subscribe(&ch).await;
        }
    }
    let mut stream = p.notifications().await?;
    let temp_offset: i8 = TEMP_OFFSET;

    if watch {
        println!("订阅中（Ctrl-C 退出）…");
    }
    let deadline = Instant::now() + Duration::from_secs(if watch { 86400 } else { 3 });

    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), stream.next()).await {
            Ok(Some(n)) => {
                let is_temp = n.uuid.to_string().eq_ignore_ascii_case(CH_TEMP);
                let text = if is_temp {
                    let raw = if n.value.len() >= 2 && n.value[0] == 0x04 {
                        n.value[1]
                    } else {
                        n.value.first().copied().unwrap_or(0)
                    };
                    format!("温度 {}°C", (raw as i8).saturating_sub(temp_offset))
                } else {
                    match decode_status(&n.value, temp_offset) {
                        Some(t) => t,
                        None => continue,
                    }
                };
                println!("{text}");
            }
            Ok(None) => break,
            Err(_) => { /* 超时继续等 */ }
        }
    }
    Ok(())
}

/// 把任意 raw 值吸附到最近的挡位（设备回读时用）
fn nearest_level(raw: u8) -> u8 {
    let mut best = 1u8;
    let mut best_diff = u8::MAX;
    for (i, v) in FAN_LEVELS.iter().enumerate() {
        let d = v.abs_diff(raw);
        if d < best_diff {
            best_diff = d;
            best = (i + 1) as u8;
        }
    }
    best
}

fn parse_color(s: &str) -> R<(u8, u8, u8)> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return Err("颜色格式应为 #rrggbb".into());
    }
    let r = u8::from_str_radix(&s[0..2], 16).map_err(|_| "颜色格式错误")?;
    let g = u8::from_str_radix(&s[2..4], 16).map_err(|_| "颜色格式错误")?;
    let b = u8::from_str_radix(&s[4..6], 16).map_err(|_| "颜色格式错误")?;
    Ok((r, g, b))
}

/* ================= 自动巡航 ================= */

/// Apple 官方热压力等级：0 正常 / 1 偏热 / 2 严重 / 3 临界
/// 这是 ProcessInfo.thermalState，读到的是 SoC 的热压力，零权限、零安装。
fn thermal_state() -> isize {
    unsafe {
        let info: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
        let state: isize = msg_send![info, thermalState];
        state
    }
}

fn heat_name(s: isize) -> &'static str {
    match s {
        0 => "正常",
        1 => "偏热",
        2 => "严重",
        _ => "临界",
    }
}

fn level_for_heat(state: isize, min_level: u8, max_level: u8) -> u8 {
    let min_level = min_level.clamp(1, 8);
    let max_level = max_level.clamp(min_level, 8);
    let base: u8 = match state {
        0 => 3,
        1 => 5,
        2 => 7,
        _ => 8,
    };
    base.max(min_level).min(max_level)
}

/// 屏保/锁屏动画进程：屏保启动和锁屏都会拉起它，比单纯看 IOConsoleLocked 更灵
fn screen_saver_running() -> bool {
    std::process::Command::new("pgrep")
        .arg("-x")
        .arg("ScreenSaverEngine")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 锁屏状态：IOConsoleLocked == Yes
fn screen_locked() -> bool {
    Command::new("ioreg")
        .args(["-n", "Root", "-d1"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            s.lines()
                .any(|l| l.contains("IOConsoleLocked") && l.contains("Yes"))
        })
        .unwrap_or(false)
}

/// 键鼠空闲秒数：HIDIdleTime（纳秒）
fn idle_seconds() -> u64 {
    Command::new("ioreg")
        .args(["-c", "IOHIDSystem"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines()
                .find(|l| l.contains("HIDIdleTime"))
                .and_then(|l| l.rsplit('=').next())
                .and_then(|v| v.trim().trim_matches('"').parse::<u64>().ok())
        })
        .map(|ns| ns / 1_000_000_000)
        .unwrap_or(0)
}

/// 取系统本地时间：(年, 月, 日, 时, 分, 秒)
/// 之前硬编码 +8 小时，在 Asia/Tokyo 的机器上整整差一小时，日志时间全错。
fn local_now() -> (i32, i32, i32, i32, i32, i32) {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
        )
    }
}

fn now_hms() -> String {
    let (_, _, _, h, m, s) = local_now();
    format!("{h:02}:{m:02}:{s:02}")
}

async fn autopilot(args: &[String]) -> R<()> {
    let get = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    let interval: u64 = get("--interval").and_then(|s| s.parse().ok()).unwrap_or(8);
    let idle_min: u64 = get("--idle-min").and_then(|s| s.parse().ok()).unwrap_or(5);
    let min_level: u8 = get("--min").and_then(|s| s.parse().ok()).unwrap_or(2);
    let max_level: u8 = get("--max").and_then(|s| s.parse().ok()).unwrap_or_else(load_auto_max);
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");

    println!("自动巡航已启动");
    println!(
        "  每 {interval}s 采样 · 空闲 {idle_min} 分钟或锁屏即停 · 档位 {min_level}-{max_level}"
    );
    println!("  Ctrl-C 退出；运行期间手机上的红魔装备库连不上（BLE 被占用）");

    let mut dev: Option<Peripheral> = None;
    let mut last_away: Option<bool> = None;
    let mut last_level: Option<u8> = None;
    let mut down_votes = 0u8;
    let mut macmon = MacmonFeed::start();
    let mut power_buf: Vec<f32> = Vec::new();
    println!(
        "  温度源：{}",
        if macmon.is_some() {
            "macmon（CPU 真实温度）"
        } else {
            "thermalState（热压力，未装 macmon）"
        }
    );

    loop {
        if dev.is_none() {
            match open().await {
                Ok(d) => {
                    println!("{} 已连接", now_hms());
                    dev = Some(d);
                    last_away = None;
                    last_level = None;
                }
                Err(e) => {
                    eprintln!("{} 连接失败：{e}", now_hms());
                    sleep(Duration::from_secs(interval.max(5))).await;
                    continue;
                }
            }
        }
        let p = dev.as_ref().unwrap();

        let locked = screen_locked();
        let idle = idle_seconds();
        let saver = screen_saver_running();
        // 屏保/锁屏即时生效，空闲按 idle_min 累计
        let away = locked || saver || (idle_min > 0 && idle >= idle_min * 60);
        let heat = thermal_state();
        let (cpu_temp, power_avg) = match macmon.as_mut() {
            Some(f) => f.next_sample().await.map(|(t, p, _)| {
                let pa = smooth_into(&mut power_buf, p, POWER_WINDOW);
                (Some(t), Some(pa))
            }).unwrap_or((None, None)),
            None => (None, None),
        };
        let target = match cpu_temp {
            Some(t) => level_for_cpu_temp(effective_temp(t, power_avg), min_level, max_level),
            None => level_for_heat(heat, min_level, max_level),
        };

        if verbose {
            println!(
                "{} 热压力={} 目标{}档 锁屏={} 空闲={}s",
                now_hms(),
                heat_name(heat),
                target,
                if locked { "是" } else { "否" },
                idle
            );
        }

        let mut failed = false;

        if away {
            if last_away != Some(true) {
                let why = if locked { "锁屏" } else { "空闲超时" };
                match write(p, CH_HALL, &[0x03]).await {
                    Ok(_) => {
                        println!("{} {} → 已关制冷", now_hms(), why);
                        last_away = Some(true);
                        last_level = None;
                        down_votes = 0;
                    }
                    Err(_) => failed = true,
                }
            }
        } else {
            if last_away == Some(true) {
                match write(p, CH_HALL, &[0x02]).await {
                    Ok(_) => println!("{} 回来了 → 已开制冷", now_hms()),
                    Err(_) => failed = true,
                }
            }
            last_away = Some(false);

            if !failed {
                match last_level {
                    None => {
                        if write(p, CH_FAN, &[FAN_LEVELS[(target - 1) as usize]])
                            .await
                            .is_ok()
                        {
                            println!(
                                "{} CPU {} → 第 {} 档",
                                now_hms(),
                                match cpu_temp {
                                    Some(t) => format!("{t:.1}°C"),
                                    None => heat_name(heat).to_string(),
                                },
                                target
                            );
                            last_level = Some(target);
                            down_votes = 0;
                        } else {
                            failed = true;
                        }
                    }
                    Some(prev) if target > prev => {
                        if write(p, CH_FAN, &[FAN_LEVELS[(target - 1) as usize]])
                            .await
                            .is_ok()
                        {
                            println!(
                                "{} 升档 {} → {}（热压力{}）",
                                now_hms(),
                                prev,
                                target,
                                heat_name(heat)
                            );
                            last_level = Some(target);
                            down_votes = 0;
                        } else {
                            failed = true;
                        }
                    }
                    Some(prev) if target < prev => {
                        // 降档要连续两轮确认，避免在阈值附近反复横跳
                        down_votes += 1;
                        if down_votes >= 2 {
                            if write(p, CH_FAN, &[FAN_LEVELS[(target - 1) as usize]])
                                .await
                                .is_ok()
                            {
                                println!(
                                    "{} 降档 {} → {}（热压力{}）",
                                    now_hms(),
                                    prev,
                                    target,
                                    heat_name(heat)
                                );
                                last_level = Some(target);
                            }
                            down_votes = 0;
                        }
                    }
                    _ => down_votes = 0,
                }
            }
        }

        if failed {
            eprintln!("{} 写入失败，准备重连", now_hms());
            if let Some(d) = dev.take() {
                let _ = d.disconnect().await;
            }
            sleep(Duration::from_secs(2)).await;
            continue;
        }

        sleep(Duration::from_secs(interval)).await;
    }
}


/* ===================== 实时面板 (TUI) ===================== */

use btleplug::api::ValueNotification;

#[derive(Default)]
struct Ui {
    device: String,
    connected: bool,
    temp: Option<i8>,
    rpm: Option<u16>,
    power: Option<u8>,
    limit: Option<u8>,
    hall: Option<bool>,
    level: Option<u8>,
    diablo: Option<bool>,
    auto_mode: Option<bool>,
    heat: isize,
    cpu_temp: Option<f32>,
    cpu_power: Option<f32>,
    locked: bool,
    idle: u64,
    autopilot: bool,
    auto_last: Option<(u8, f32)>,
    manual_until: Option<Instant>,
    auto_max: u8,
    temp_buf: Vec<f32>,
    power_buf: Vec<f32>,
    down_votes: u8,
    log: Vec<String>,
}

impl Ui {
    fn log(&mut self, msg: impl Into<String>) {
        self.log.push(format!("{}  {}", now_hms(), msg.into()));
        while self.log.len() > 40 {
            self.log.remove(0);
        }
    }

}









fn apply_frame(n: ValueNotification, ui: &mut Ui) {
    let is_temp = n.uuid.to_string().eq_ignore_ascii_case(CH_TEMP);
    let b = &n.value;
    if b.is_empty() {
        return;
    }
    if is_temp {
        let raw = if b.len() >= 2 && b[0] == 0x04 { b[1] } else { b[0] };
        let t = (raw as i8).saturating_sub(TEMP_OFFSET);
        ui.temp = Some(t);
        return;
    }
    match b[0] {
        0x04 => {
            let t = (b.get(1).copied().unwrap_or(0) as i8).saturating_sub(TEMP_OFFSET);
            ui.temp = Some(t);
        }
        0x05 => {}
        0x07 => ui.limit = Some(b.get(1).copied().unwrap_or(0)),
        0x08 => {
            let hi = b.get(1).copied().unwrap_or(0) as u16;
            let lo = b.get(2).copied().unwrap_or(0) as u16;
            let rpm = (hi << 8) | lo;
            ui.rpm = Some(rpm);
        }
        0x09 => {
            let w = b.get(1).copied().unwrap_or(0);
            ui.power = Some(w);
        }
        _ => {}
    }
}



/* ===================== 配置文件 ===================== */
// ~/.rmcooler/config，每行 key=value。只存用户显式设置过的偏好（如自动上限），
// 未设置的项用代码内默认值。

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".rmcooler").join("config")
}

fn config_get(key: &str) -> Option<String> {
    let content = std::fs::read_to_string(config_path()).ok()?;
    content
        .lines()
        .find_map(|l| l.split_once('=').filter(|(k, _)| *k == key))
        .map(|(_, v)| v.to_string())
}

fn config_set(key: &str, value: &str) {
    if let Some(dir) = config_path().parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let mut lines: Vec<String> = std::fs::read_to_string(config_path())
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.split_once('=').map_or(false, |(k, _)| k == key))
        .map(|l| l.to_string())
        .collect();
    lines.push(format!("{key}={value}"));
    let _ = std::fs::write(config_path(), lines.join("\n") + "\n");
}

/// 自动调档上限：配置文件优先，未设置默认 8
fn load_auto_max() -> u8 {
    config_get("auto_max")
        .and_then(|v| v.parse::<u8>().ok())
        .filter(|l| (1..=8).contains(l))
        .unwrap_or(8)
}

/* ===================== 单实例锁 ===================== */
// 两个实例会互抢同一条 BLE 连接，表现成「按键时灵时不灵」。
// 锁文件记录 PID，启动时用 kill -0 确认对方还活着才算冲突。

fn lock_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".rmcooler").join("lock")
}

fn acquire_lock(mode: &str) -> Result<(), String> {
    std::fs::create_dir_all(lock_path().parent().unwrap()).ok();
    let path = lock_path();
    if let Ok(content) = std::fs::read_to_string(&path) {
        let parts: Vec<&str> = content.split_whitespace().collect();
        if let Some(pid_str) = parts.first() {
            if let Ok(pid) = pid_str.parse::<i32>() {
                let alive = std::process::Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if alive && pid != std::process::id() as i32 {
                    let old_mode = parts.get(1).copied().unwrap_or("?");
                    let hint = if old_mode == "menubar" {
                        "状态栏已在运行，点屏幕右上角的温度数字即可操作。"
                    } else {
                        "先 `hm service stop` 或退出它再启动。"
                    };
                    return Err(format!(
                        "另一个 rmcooler 正在运行（PID {pid}，{old_mode}）。{hint}"
                    ));
                }
            }
        }
    }
    std::fs::write(&path, format!("{} {mode}", std::process::id()))
        .map_err(|e| format!("写锁文件失败: {e}"))
}

fn release_lock() {
    let _ = std::fs::remove_file(lock_path());
}

struct LockGuard;
impl Drop for LockGuard {
    fn drop(&mut self) {
        release_lock();
    }
}

/* ===================== BLE 工作线程 ===================== */
// 核心原则：UI 线程绝不直接 await BLE。
// 设备一次写要 3 秒、读要 3 秒，放在 UI 线程里就是界面冻结；
// 连按几个档位会连环冻结，表现出来就是「卡死」。
// 所以这里把 Peripheral 的所有权交给一个常驻 worker，UI 只发命令、只收消息。

#[derive(Debug)]
enum BleCmd {
    Fan(u8),
    Hall(bool),
    Diablo(bool),
    Auto(bool),
    Reconnect,
}

enum BleMsg {
    Log(String),
    Connected(String),
    Disconnected,
    Frame(ValueNotification),
    State {
        hall: Option<bool>,
        level: Option<u8>,
        diablo: Option<bool>,
        auto_mode: Option<bool>,
    },
    Done {
        desc: String,
        err: Option<String>,
    },
}

struct InitialState {
    hall: Option<bool>,
    level: Option<u8>,
    diablo: Option<bool>,
    auto_mode: Option<bool>,
}

/// 回读当前状态。每个 read 都带超时，设备不回应就跳过。
async fn read_initial_state(p: &Peripheral) -> InitialState {
    let mut st = InitialState {
        hall: None,
        level: None,
        diablo: None,
        auto_mode: None,
    };

    if let Ok(ch) = find(p, CH_HALL) {
        if let Ok(Ok(v)) = tokio::time::timeout(Duration::from_secs(3), p.read(&ch)).await {
            if !v.is_empty() {
                st.hall = Some(v[0] == 0x02);
            }
        }
    }
    if let Ok(ch) = find(p, CH_FAN) {
        if let Ok(Ok(v)) = tokio::time::timeout(Duration::from_secs(3), p.read(&ch)).await {
            if !v.is_empty() {
                st.level = Some(nearest_level(v[0]));
            }
        }
    }
    if let Ok(ch) = find(p, CH_DIABLO) {
        if let Ok(Ok(v)) = tokio::time::timeout(Duration::from_secs(3), p.read(&ch)).await {
            if !v.is_empty() {
                st.diablo = Some(v[0] != 0);
            }
        }
    }
    if let Ok(ch) = find(p, CH_AUTO) {
        if let Ok(Ok(v)) = tokio::time::timeout(Duration::from_secs(3), p.read(&ch)).await {
            if !v.is_empty() {
                st.auto_mode = Some(v[0] != 0);
            }
        }
    }

    st
}

/// 执行一条命令，返回连接是否还活着
async fn execute_cmd(
    p: &Peripheral,
    cmd: BleCmd,
    tx: &tokio::sync::mpsc::Sender<BleMsg>,
) -> bool {
    let (desc, res) = match cmd {
        // 重连在外层处理，这里只是让 match 完备
        BleCmd::Reconnect => ("重连".to_string(), Ok(())),
        BleCmd::Fan(level) => (
            format!("第 {level} 档"),
            write(p, CH_FAN, &[FAN_LEVELS[(level - 1) as usize]]).await,
        ),
        BleCmd::Hall(on) => (
            if on {
                "制冷 开".to_string()
            } else {
                "制冷 关".to_string()
            },
            write(p, CH_HALL, &[if on { 0x02 } else { 0x03 }]).await,
        ),
        BleCmd::Diablo(on) => {
            let mut pre = None;
            if on {
                if let Err(e) = write(p, CH_AUTO, &[0x00]).await {
                    pre = Some(e);
                }
            }
            let r = write(p, CH_DIABLO, &[if on { 0x01 } else { 0x00 }]).await;
            (
                format!("破坏神 {}", if on { "开" } else { "关" }),
                match pre {
                    Some(e) => Err(e),
                    None => r,
                },
            )
        }
        BleCmd::Auto(on) => {
            let mut pre = None;
            if on {
                if let Err(e) = write(p, CH_DIABLO, &[0x00]).await {
                    pre = Some(e);
                }
            }
            let r = write(p, CH_AUTO, &[if on { 0x01 } else { 0x00 }]).await;
            (
                format!("智能温控 {}", if on { "开" } else { "关" }),
                match pre {
                    Some(e) => Err(e),
                    None => r,
                },
            )
        }
    };

    let alive = match &res {
        Ok(_) => true,
        Err(_) => p.is_connected().await.unwrap_or(false),
    };

    let _ = tx
        .send(BleMsg::Done {
            desc,
            err: res.err().map(|e| e.to_string()),
        })
        .await;

    alive
}

/// 常驻 worker：连接 → 订阅 → 回读 → 循环处理命令与通知；断开则自动重连。
async fn ble_worker(
    initial: Option<Peripheral>,
    mut cmd_rx: tokio::sync::mpsc::Receiver<BleCmd>,
    tx: tokio::sync::mpsc::Sender<BleMsg>,
) {
    let mut pending = initial;
    let mut fails: u32 = 0;

    loop {
        let p = match pending.take() {
            Some(p) => p,
            None => {
                let _ = tx.send(BleMsg::Log("正在重新连接…".into())).await;
                match open().await {
                    Ok(p) => {
                        fails = 0;
                        // 丢掉睡眠/断线期间积压的旧命令，避免醒来后突然跳档
                        while cmd_rx.try_recv().is_ok() {}
                        p
                    }
                    Err(e) => {
                        fails += 1;
                        // 限流：只报第 1 次，之后每分钟报一次，别刷屏
                        if fails == 1 || fails % 12 == 0 {
                            let _ = tx
                                .send(BleMsg::Log(format!("连接失败（第 {fails} 次）：{e}")))
                                .await;
                        }
                        // 退避递增：5s → 7s → … → 15s，别把设备刷爆
                        let backoff = (5 + fails as u64 * 2).min(15);
                        tokio::select! {
                            _ = sleep(Duration::from_secs(backoff)) => {}
                            _ = cmd_rx.recv() => {}
                        }
                        continue;
                    }
                }
            }
        };

        let _ = tx
            .send(BleMsg::Connected(device_label(&p).await))
            .await;

        for uuid in [CH_STATUS, CH_TEMP] {
            if let Ok(ch) = find(&p, uuid) {
                match tokio::time::timeout(Duration::from_secs(5), p.subscribe(&ch)).await {
                    Ok(Ok(_)) => {
                        let _ = tx
                            .send(BleMsg::Log(format!("已订阅 {}", short_uuid(uuid))))
                            .await;
                    }
                    Ok(Err(e)) => {
                        let _ = tx
                            .send(BleMsg::Log(format!(
                                "订阅 {} 失败: {e}",
                                short_uuid(uuid)
                            )))
                            .await;
                    }
                    Err(_) => {
                        let _ = tx
                            .send(BleMsg::Log(format!(
                                "订阅 {} 超时",
                                short_uuid(uuid)
                            )))
                            .await;
                    }
                }
            }
        }

        let st = read_initial_state(&p).await;
        let got = [st.hall, st.level.map(|_| true), st.diablo, st.auto_mode]
            .iter()
            .filter(|x| x.is_some())
            .count();
        let _ = tx
            .send(BleMsg::Log(format!("回读完成（{got}/4 项成功）")))
            .await;
        let _ = tx
            .send(BleMsg::State {
                hall: st.hall,
                level: st.level,
                diablo: st.diablo,
                auto_mode: st.auto_mode,
            })
            .await;

        let mut notif =
            match tokio::time::timeout(Duration::from_secs(5), p.notifications()).await {
                Ok(Ok(s)) => s,
                _ => {
                    let _ = tx.send(BleMsg::Disconnected).await;
                    continue;
                }
            };

        // 会话循环。心跳是必须的：Mac 睡眠时进程被挂起，BLE 链路已经断了，
        // 但通知流既不报错也不结束，没人主动问就一直以为还连着。
        let mut health = tokio::time::interval(Duration::from_secs(5));
        health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut silent_ticks = 0u32;

        loop {
            let cmd = tokio::select! {
                n = notif.next() => {
                    match n {
                        Some(n) => {
                            silent_ticks = 0;
                            let _ = tx.send(BleMsg::Frame(n)).await;
                            continue;
                        }
                        None => {
                            let _ = tx.send(BleMsg::Disconnected).await;
                            break;
                        }
                    }
                }
                _ = health.tick() => {
                    // is_connected 本身也可能卡在蓝牙栈里，必须带超时
                    let alive = matches!(
                        tokio::time::timeout(Duration::from_secs(2), p.is_connected()).await,
                        Ok(Ok(true))
                    );
                    if !alive {
                        let _ = tx.send(BleMsg::Log("链路已失效，准备重连".into())).await;
                        let _ = tx.send(BleMsg::Disconnected).await;
                        let _ = p.disconnect().await;
                        break;
                    }
                    // 链路还在但长时间没有数据：只提示，不强断
                    silent_ticks += 1;
                    if silent_ticks == 12 {
                        let _ = tx.send(BleMsg::Log("已 60 秒没有收到设备数据".into())).await;
                    }
                    continue;
                }
                c = cmd_rx.recv() => c,
            };

            match cmd {
                Some(BleCmd::Reconnect) => {
                    let _ = tx.send(BleMsg::Log("手动重连…".into())).await;
                    let _ = tx.send(BleMsg::Disconnected).await;
                    let _ = p.disconnect().await;
                    break;
                }
                Some(c) => {
                    if !execute_cmd(&p, c, &tx).await {
                        let _ = tx.send(BleMsg::Disconnected).await;
                        let _ = p.disconnect().await;
                        break;
                    }
                }
                None => return, // UI 已退出
            }
        }
    }
}

fn apply_msg(msg: BleMsg, ui: &mut Ui) {
    match msg {
        BleMsg::Log(s) => ui.log(s),
        BleMsg::Connected(name) => {
            ui.device = name;
            ui.connected = true;
            ui.log("已连接");
        }
        BleMsg::Disconnected => {
            ui.connected = false;
            ui.device = "重连中…".to_string();
            ui.log("连接已断开，正在重连…");
        }
        BleMsg::Frame(n) => apply_frame(n, ui),
        BleMsg::State {
            hall,
            level,
            diablo,
            auto_mode,
        } => {
            if hall.is_some() {
                ui.hall = hall;
            }
            if level.is_some() {
                ui.level = level;
            }
            if diablo.is_some() {
                ui.diablo = diablo;
            }
            if auto_mode.is_some() {
                ui.auto_mode = auto_mode;
            }
        }
        BleMsg::Done { desc, err } => match err {
            Some(e) => ui.log(format!("{desc} 失败：{e}")),
            None => ui.log(desc),
        },
    }
}

/// 按键处理：只发命令 + 乐观更新，绝不 await BLE

fn short_uuid(u: &str) -> String {
    u.chars().skip(4).take(4).collect()
}

/// macOS 上 btleplug 的 address() 常是全零，改用广播名
async fn device_label(p: &Peripheral) -> String {
    match tokio::time::timeout(Duration::from_secs(2), p.properties()).await {
        Ok(Ok(Some(props))) => props
            .local_name
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| p.address().to_string()),
        _ => p.address().to_string(),
    }
}



/* ===================== 数据摘要 ===================== */
// 读当日 CSV，输出一份人能直接看的摘要，不用再开 Excel

fn report(date: &str) -> R<()> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let path = PathBuf::from(home)
        .join(".rmcooler")
        .join(format!("history-{date}.csv"));

    if !path.exists() {
        return Err(format!("{date} 没有记录（文件不存在：{}）", path.display()).into());
    }

    let content = std::fs::read_to_string(&path)?;
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in content.lines().skip(1) {
        let cols: Vec<String> = line.split(',').map(|x| x.to_string()).collect();
        if cols.len() >= 10 && !cols[0].is_empty() {
            rows.push(cols);
        }
    }
    if rows.is_empty() {
        return Err(format!("{date} 有文件但没有采样数据").into());
    }

    let f32_of = |c: &str| c.parse::<f32>().ok();
    let temps: Vec<f32> = rows.iter().filter_map(|r| f32_of(&r[1])).collect();
    let rpms: Vec<f32> = rows.iter().filter_map(|r| f32_of(&r[2])).collect();
    let powers: Vec<f32> = rows.iter().filter_map(|r| f32_of(&r[3])).collect();
    // CSV 列序：0=time 1=radiator_c 2=rpm 3=power_w 4=level 5=cooling 6=cpu_c 7=cpu_w
    let cpu: Vec<f32> = rows.iter().filter_map(|r| f32_of(&r[6])).collect();

    fn stats(v: &[f32]) -> (Option<f32>, Option<f32>, Option<f32>) {
        if v.is_empty() {
            return (None, None, None);
        }
        let mn = v.iter().cloned().fold(f32::INFINITY, f32::min);
        let mx = v.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let avg = v.iter().sum::<f32>() / v.len() as f32;
        (Some(mn), Some(mx), Some(avg))
    }
    let (t_min, t_max, t_avg) = stats(&temps);
    let (_, r_max, r_avg) = stats(&rpms);
    let (_, p_max, p_avg) = stats(&powers);
    let (c_min, c_max, c_avg) = stats(&cpu);

    // 运行时长（首行到末行，按秒）
    let to_secs = |s: &str| -> Option<u32> {
        let p: Vec<u32> = s.split(':').filter_map(|x| x.parse().ok()).collect();
        if p.len() == 3 {
            Some(p[0] * 3600 + p[1] * 60 + p[2])
        } else {
            None
        }
    };
    let dur = to_secs(&rows[0][0])
        .zip(to_secs(&rows[rows.len() - 1][0]))
        .map(|(a, b)| b.saturating_sub(a))
        .unwrap_or(0);

    // 档位分布
    let mut dist = [0u32; 9];
    for r in &rows {
        if let Ok(l) = r[4].parse::<usize>() {
            if (1..=8).contains(&l) {
                dist[l] += 1;
            }
        }
    }

    // 关制冷次数：cooling 从 1 → 0
    let mut off_count = 0;
    let mut prev = true;
    for r in &rows {
        let cur = r[5] == "1";
        if prev && !cur {
            off_count += 1;
        }
        prev = cur;
    }

    let fm = |v: Option<f32>| v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "—".into());

    println!("═══ 红魔散热器 · {date} 摘要 ═══");
    println!(
        "采样 {} 条 · 跨度 {} 分 {} 秒",
        rows.len(),
        dur / 60,
        dur % 60
    );
    println!();
    println!(
        "制冷片温度  低 {} / 均 {} / 高 {} °C",
        fm(t_min),
        fm(t_avg),
        fm(t_max)
    );
    println!(
        "风扇转速    低 {} / 均 {} / 高 {} rpm",
        r_max
            .map(|_| format!("{:.0}", r_avg.unwrap_or(0.0)))
            .unwrap_or_else(|| "—".into()),
        fm(r_avg),
        r_max.map(|x| format!("{x:.0}")).unwrap_or_else(|| "—".into())
    );
    println!(
        "散热器功率  均 {} / 峰 {} W",
        fm(p_avg),
        p_max.map(|x| format!("{x:.0}")).unwrap_or_else(|| "—".into())
    );
    println!(
        "CPU 温度    低 {} / 均 {} / 峰 {} °C",
        fm(c_min),
        fm(c_avg),
        c_max.map(|x| format!("{x:.1}")).unwrap_or_else(|| "—".into())
    );
    println!();
    print!("档位分布：");
    for l in 1..=8 {
        if dist[l] > 0 {
            print!("  {}档 {:.0}%", l, dist[l] as f32 / rows.len() as f32 * 100.0);
        }
    }
    println!();
    println!("自动关制冷 {} 次", off_count);

    Ok(())
}

/* ===================== 后台服务 ===================== */

fn service_plist_path() -> R<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| "没有 HOME")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join("com.rmcooler.plist"))
}

fn service_log_path() -> R<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| "没有 HOME")?;
    Ok(PathBuf::from(home).join(".rmcooler").join("service.log"))
}

fn service_install() -> R<()> {
    let bin = std::env::current_exe().map_err(|e| format!("找不到自身路径: {e}"))?;
    let plist = service_plist_path()?;
    let log = service_log_path()?;
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).ok();
    }

    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.rmcooler</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>autopilot</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
</dict>
</plist>
"#,
        bin.display(),
        log.display(),
        log.display(),
    );
    std::fs::write(&plist, content).map_err(|e| format!("写 plist 失败: {e}"))?;

    let out = std::process::Command::new("launchctl")
        .args(["load", &plist.display().to_string()])
        .output()
        .map_err(|e| format!("launchctl 失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "launchctl load 失败: {}",
            String::from_utf8_lossy(&out.stderr).to_string()
        ).into());
    }

    println!("开机自启已安装（后台 autopilot 模式，无界面）");
    println!("  配置：{}", plist.display());
    println!("  日志：{}", log.display());
    println!();
    println!("注意：后台服务会占住蓝牙，此时 hm 面板起不来。");
    println!("想用面板：rmcooler service stop");
    println!("开机自启会保留，下次登录继续。");
    Ok(())
}

fn service_uninstall() -> R<()> {
    let plist = service_plist_path()?;
    if plist.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist.display().to_string()])
            .output();
        std::fs::remove_file(&plist).map_err(|e| format!("删 plist 失败: {e}"))?;
        println!("开机自启已卸载");
    } else {
        println!("没有安装过开机自启");
    }
    Ok(())
}

fn service_stop() -> R<()> {
    let plist = service_plist_path()?;
    if plist.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist.display().to_string()])
            .output();
        let _ = std::process::Command::new("launchctl")
            .args(["load", &plist.display().to_string()])
            .output();
        // unload/load 之间 launchd 可能已经把进程拉起来又杀了，再兜底一次
        let _ = std::process::Command::new("pkill")
            .args(["-f", "rmcooler autopilot"])
            .output();
        println!("后台服务已停止（配置保留，重启电脑或 service start 会再跑）");
    } else {
        let _ = std::process::Command::new("pkill")
            .args(["-f", "rmcooler autopilot"])
            .output();
        println!("没有安装开机自启；已尝试结束正在跑的 autopilot 进程");
    }
    Ok(())
}

/* ===================== 纯文本模式 ===================== */





/* ===================== 本机温度：复用 macmon ===================== */
// macmon 用 IOReport + SMC 私有 API 读真实温度，免 sudo。
// 这里不重复实现那套 FFI，而是常驻一个 `macmon pipe` 子进程读它的 JSON 流：
// 只 fork 一次，之后每秒一行，开销极低。

use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::ChildStdout;

/// 从一行 JSON 里取浮点字段（避免引入 serde_json）
fn json_f32(json: &str, key: &str) -> Option<f32> {
    let pat = format!("\"{key}\":");
    let i = json.find(&pat)? + pat.len();
    let rest = &json[i..];
    let end = rest.find([',', '}'])?;
    rest[..end].trim().parse().ok()
}

fn macmon_bin() -> Option<String> {
    for p in ["/opt/homebrew/bin/macmon", "/usr/local/bin/macmon"] {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    None
}

struct MacmonFeed {
    child: tokio::process::Child,
    lines: Lines<BufReader<ChildStdout>>,
}

impl MacmonFeed {
    fn start() -> Option<Self> {
        let bin = macmon_bin()?;
        let mut child = tokio::process::Command::new(bin)
            .args(["pipe", "-i", "1000"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let out = child.stdout.take()?;
        Some(Self {
            child,
            lines: BufReader::new(out).lines(),
        })
    }

    /// 返回 (CPU 温度, CPU 功耗, 整机功耗)
    async fn next_sample(&mut self) -> Option<(f32, f32, f32)> {
        while let Ok(Some(line)) = self.lines.next_line().await {
            if let Some(t) = json_f32(&line, "cpu_temp_avg") {
                return Some((
                    t,
                    json_f32(&line, "cpu_power").unwrap_or(0.0),
                    json_f32(&line, "sys_power").unwrap_or(0.0),
                ));
            }
        }
        None
    }
}

impl Drop for MacmonFeed {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn next_macmon(f: &mut Option<MacmonFeed>) -> Option<(f32, f32, f32)> {
    if f.is_none() {
        return std::future::pending().await;
    }
    let res = f.as_mut().unwrap().next_sample().await;
    match res {
        Some(s) => Some(s),
        None => {
            // 子进程结束：永久 pending，避免 select 忙循环
            *f = None;
            std::future::pending().await
        }
    }
}


/* ---- 自动调档的防抖：施密特触发 + 降档确认 ----
   原来的实现是「温度变化超过 2°C 就允许换档」，但温度本来就在阈值附近自然波动，
   等于没做迟滞。实测会出现 2 档↔4 档每 2 秒来回切。
   现在改成：升档要超过当前档位上界 +3°C，降档要低于下界 -3°C，
   且降档还必须连续 10 秒都满足，避免一次毛刺就掉档。 */

// 偏保守策略：天毅明确说「不怕太凉，就怕 CPU 飘上去」。
// 升档即时，降档必须持续 60 秒低于「下界 − 5°C」才放行——
// 温度天然会在 58–79 之间波动，10 秒确认根本压不住这种呼吸。
const HYST: f32 = 5.0;
const DOWN_CONFIRM_TICKS: u8 = 60;

// 功耗信号（窗口均值；M 系列 Air：空闲 1–3W，中载 5–10W，重载 15W+）
// 重载：判定温度上浮 → 提前升档；空闲：判定温度下调 + 冻结降档投票
const POWER_HIGH: f32 = 12.0;
const POWER_LOW: f32 = 4.0;
const POWER_WINDOW: usize = 5;

/// 每个档位对应的温度区间（1/2/4/6/8 来自映射表，3/5/7 为手动档插值）
fn level_bounds(level: u8) -> (f32, f32) {
    match level {
        1 => (0.0, 45.0),
        2 => (45.0, 50.0),
        3 => (50.0, 55.0),
        4 => (55.0, 60.0),
        5 => (60.0, 65.0),
        6 => (65.0, 70.0),
        7 => (70.0, 75.0),
        _ => (75.0, 120.0),
    }
}

/// 滑动平均，压掉 macmon 每秒采样的尖峰
fn smooth(buf: &mut Vec<f32>, t: f32) -> f32 {
    smooth_into(buf, t, 3)
}

fn smooth_into(buf: &mut Vec<f32>, t: f32, window: usize) -> f32 {
    buf.push(t);
    while buf.len() > window {
        buf.remove(0);
    }
    buf.iter().sum::<f32>() / buf.len() as f32
}

/// 功耗修正后的判定温度：重载上浮 5°C（提前升档），空闲下调 3°C（更难升档）。
/// 功耗必须用窗口均值——单点功耗一个尖峰就会误判。
fn effective_temp(smoothed: f32, power_avg: Option<f32>) -> f32 {
    match power_avg {
        Some(p) if p >= POWER_HIGH => smoothed + 5.0,
        Some(p) if p <= POWER_LOW => smoothed - 3.0,
        _ => smoothed,
    }
}

/// 带迟滞的目标档位：只有温度真的走出当前档位的舒适区才换
fn level_with_hysteresis(current: Option<u8>, temp: f32, min_level: u8, max_level: u8) -> u8 {
    let raw = level_for_cpu_temp(temp, min_level, max_level);
    let Some(cur) = current else {
        return raw;
    };
    if raw == cur {
        return cur;
    }
    let (lo, hi) = level_bounds(cur);
    if raw > cur {
        if temp >= hi + HYST {
            raw
        } else {
            cur
        }
    } else if temp <= lo - HYST {
        raw
    } else {
        cur
    }
}

/// CPU 温度 → 风扇档位。MacBook Air 空闲 40–50，重载 80+。
fn level_for_cpu_temp(t: f32, min_level: u8, max_level: u8) -> u8 {
    let min_level = min_level.clamp(1, 8);
    let max_level = max_level.clamp(min_level, 8);
    let base: u8 = if t < 45.0 {
        1
    } else if t < 50.0 {
        2
    } else if t < 55.0 {
        3
    } else if t < 60.0 {
        4
    } else if t < 65.0 {
        5
    } else if t < 70.0 {
        6
    } else if t < 75.0 {
        7
    } else {
        8
    };
    base.max(min_level).min(max_level)
}


/* ===================== 历史日志 ===================== */
// 写 ~/.rmcooler/history-YYYY-MM-DD.csv，每 10 秒一行，按天分文件。
// CSV 而不是 JSONL：他要用 Excel / Python 直接看曲线。

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;

/// Howard Hinnant 的 civil_from_days，避免引入 chrono
fn today_str() -> String {
    let (y, m, d, ..) = local_now();
    format!("{y:04}-{m:02}-{d:02}")
}

struct History {
    dir: PathBuf,
    path: PathBuf,
    last: Instant,
    interval: Duration,
}

impl History {
    fn new(interval_secs: u64) -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        Self::at(PathBuf::from(home).join(".rmcooler"), interval_secs)
    }

    fn at(dir: PathBuf, interval_secs: u64) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let h = Self {
            dir: dir.clone(),
            path: dir.join("history-x.csv"),
            last: Instant::now(),
            interval: Duration::from_secs(1),
        };
        h.cleanup_old(7);
        let path = dir.join(format!("history-{}.csv", today_str()));
        Self {
            dir,
            path,
            last: Instant::now(),
            interval: Duration::from_secs(interval_secs.max(1)),
        }
    }

    /// 删掉 keep_days 天前的日志文件
    fn cleanup_old(&self, keep_days: i64) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let cutoff_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64 - keep_days * 86400)
            .unwrap_or(0);
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            let t = cutoff_secs;
            libc::localtime_r(&t, &mut tm);
            let cutoff = format!(
                "{:04}-{:02}-{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday
            );
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(date) = name
                    .strip_prefix("history-")
                    .and_then(|x| x.strip_suffix(".csv"))
                {
                    // YYYY-MM-DD 字典序即时间序
                    if date < cutoff.as_str() {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
    }


    fn write_row(&mut self, ui: &Ui) {
        // 跨天自动换文件
        let today = today_str();
        let want = self.dir.join(format!("history-{today}.csv"));
        if want != self.path {
            self.path = want;
        }

        let header_needed = !self.path.exists();
        let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };

        if header_needed {
            let _ = writeln!(
                f,
                "time,radiator_c,rpm,power_w,level,cooling,cpu_c,cpu_w,locked,autopilot"
            );
        }

        let num = |v: Option<i8>| v.map(|x| x.to_string()).unwrap_or_default();
        let numu = |v: Option<u16>| v.map(|x| x.to_string()).unwrap_or_default();
        let num8 = |v: Option<u8>| v.map(|x| x.to_string()).unwrap_or_default();
        let flag = |v: Option<bool>| match v {
            Some(true) => "1",
            Some(false) => "0",
            None => "",
        };
        let optf = |v: Option<f32>| v.map(|x| format!("{x:.1}")).unwrap_or_default();

        let _ = writeln!(
            f,
            "{},{},{},{},{},{},{},{},{},{}",
            now_hms(),
            num(ui.temp),
            numu(ui.rpm),
            num8(ui.power),
            num8(ui.level),
            flag(ui.hall),
            optf(ui.cpu_temp),
            optf(ui.cpu_power),
            if ui.locked { "1" } else { "0" },
            if ui.autopilot { "1" } else { "0" }
        );
        self.last = Instant::now();
    }

    fn maybe_write(&mut self, ui: &Ui) {
        if self.last.elapsed() >= self.interval {
            self.write_row(ui);
        }
    }
}

/* ===================== 原始帧抓包 ===================== */

fn now_ms() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let _ = d;
    let (_, _, _, h, mi, s) = local_now();
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        h,
        mi,
        s,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_millis())
            .unwrap_or(0)
    )
}

/// 把所有 Notify 特征的原样字节打出来。用于校准温度偏移、确认功率帧是否存在。
async fn raw_watch(args: &[String]) -> R<()> {
    let secs: u64 = args
        .iter()
        .position(|a| a == "--for")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    let p = open().await?;

    step("订阅所有可通知的特征");
    let mut count = 0;
    for ch in p.characteristics() {
        if ch.properties.contains(btleplug::api::CharPropFlags::NOTIFY) {
            match tokio::time::timeout(Duration::from_secs(4), p.subscribe(&ch)).await {
                Ok(Ok(_)) => {
                    step(&format!("已订阅 {}", short_uuid(&ch.uuid.to_string())));
                    count += 1;
                }
                Ok(Err(e)) => step(&format!("{} 订阅失败: {e}", short_uuid(&ch.uuid.to_string()))),
                Err(_) => step(&format!("{} 订阅超时", short_uuid(&ch.uuid.to_string()))),
            }
        }
    }
    if count == 0 {
        return Err("没有任何特征支持通知，无法抓包".into());
    }

    let mut stream = match tokio::time::timeout(Duration::from_secs(5), p.notifications()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("获取通知流失败: {e}").into()),
        Err(_) => return Err("获取通知流超时".into()),
    };

    println!();
    println!("抓包 {secs} 秒，原样输出（Ctrl-C 提前结束）");
    println!("时间           特征   原始字节");
    println!("──────────────────────────────────────────");

    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left.min(Duration::from_secs(1)), stream.next()).await {
            Ok(Some(n)) => {
                println!(
                    "{}  {}   {}",
                    now_ms(),
                    short_uuid(&n.uuid.to_string()),
                    hex(&n.value)
                );
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }

    println!("──────────────────────────────────────────");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;



    #[test]
    fn status_frames_decode() {
        use btleplug::api::ValueNotification;
        use uuid::Uuid;

        let mk = |uuid: &str, bytes: &[u8]| ValueNotification {
            uuid: Uuid::parse_str(uuid).unwrap(),
            value: bytes.to_vec(),
        };

        let mut ui = Ui::default();
        // 1015: 温度 / 限档 / 转速 / 功率
        apply_frame(mk(CH_STATUS, &[0x04, 3, 0, 0]), &mut ui);
        assert_eq!(ui.temp, Some(-3)); // 3 - 6
        apply_frame(mk(CH_STATUS, &[0x07, 6, 0, 0]), &mut ui);
        assert_eq!(ui.limit, Some(6));
        apply_frame(mk(CH_STATUS, &[0x08, 0x13, 0xec, 0]), &mut ui);
        assert_eq!(ui.rpm, Some(5100));
        apply_frame(mk(CH_STATUS, &[0x09, 10, 0, 0]), &mut ui);
        assert_eq!(ui.power, Some(10));
        // 1014: 单字节有符号温度
        apply_frame(mk(CH_TEMP, &[-3i8 as u8]), &mut ui);
        assert_eq!(ui.temp, Some(-9)); // -3 - 6
        // 1014 固件 8.4.7 的 [0x04, temp] 包
        apply_frame(mk(CH_TEMP, &[0x04, 5]), &mut ui);
        assert_eq!(ui.temp, Some(-1)); // 5 - 6
    }

    #[test]
    fn fan_level_snapping() {
        assert_eq!(nearest_level(0x2e), 1);
        assert_eq!(nearest_level(0x50), 8);
        assert_eq!(nearest_level(0x42), 4); // 0x42=66，与 64/68 等距，取前一个
        assert_eq!(nearest_level(200), 8); // 越界也不 panic
    }

    #[test]
    fn heat_mapping() {
        assert_eq!(level_for_heat(0, 2, 8), 3);
        assert_eq!(level_for_heat(1, 2, 8), 5);
        assert_eq!(level_for_heat(2, 2, 8), 7);
        assert_eq!(level_for_heat(3, 2, 8), 8);
        // 夹取
        assert_eq!(level_for_heat(0, 5, 8), 5);
        assert_eq!(level_for_heat(3, 1, 6), 6);
    }

    #[test]
    fn history_writes_csv_row() {
        let dir = std::env::temp_dir().join("rmcooler-csv-test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut h = History::at(dir.clone(), 1);

        let mut ui = Ui::default();
        ui.temp = Some(11);
        ui.rpm = Some(4980);
        ui.power = Some(26);
        ui.level = Some(5);
        ui.hall = Some(true);
        ui.cpu_temp = Some(46.0);
        ui.cpu_power = Some(2.1);
        ui.locked = false;
        ui.autopilot = true;

        h.write_row(&ui);
        let content = std::fs::read_to_string(&h.path).unwrap();
        println!("\n{content}");

        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "一行表头 + 一行数据");
        assert_eq!(
            lines[0],
            "time,radiator_c,rpm,power_w,level,cooling,cpu_c,cpu_w,locked,autopilot"
        );
        let cols: Vec<&str> = lines[1].split(',').collect();
        assert_eq!(cols.len(), 10, "数据和表头列数必须一致");
        assert_eq!(&cols[1..], &["11", "4980", "26", "5", "1", "46.0", "2.1", "0", "1"]);

        // 空值不能写成 0
        let mut empty = Ui::default();
        h.write_row(&empty);
        let content = std::fs::read_to_string(&h.path).unwrap();
        let last = content.lines().last().unwrap();
        assert!(last.contains(",0,0"), "锁屏/自动为 0，其余空: {last}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn date_string_shape() {
        let d = today_str();
        let parts: Vec<&str> = d.split('-').collect();
        assert_eq!(parts.len(), 3, "日期应为 YYYY-MM-DD");
        assert_eq!(parts[0].len(), 4);
        assert_eq!(parts[1].len(), 2);
        assert_eq!(parts[2].len(), 2);
        let m: u32 = parts[1].parse().unwrap();
        let day: u32 = parts[2].parse().unwrap();
        assert!((1..=12).contains(&m) && (1..=31).contains(&day));
    }

    #[test]
    fn hysteresis_stops_flapping() {
        // 当前 2 档（区间 45–50），上界 50，迟滞 5 → ≥55 才升
        assert_eq!(level_with_hysteresis(Some(2), 54.9, 2, 8), 2);
        assert_eq!(level_with_hysteresis(Some(2), 55.0, 2, 8), 4); // 55.0 恰好跨入 4 档区间
        // 当前 3 档（区间 50–55），下界 50，迟滞 5 → ≤45 才降
        assert_eq!(level_with_hysteresis(Some(3), 45.1, 2, 8), 3);
        assert_eq!(level_with_hysteresis(Some(3), 45.0, 2, 8), 2);
        // 没有历史时直接用原始映射
        assert_eq!(level_with_hysteresis(None, 60.0, 2, 8), 5); // 无历史直接映射（60 ∈ [60,65) → 5 档）
        // 上限夹取生效：上限 4 时不会升到 5
        assert_eq!(level_with_hysteresis(Some(4), 90.0, 2, 4), 4);
    }

    #[test]
    fn hysteresis_survives_oscillation() {
        // 模拟他日志里的真实抖动序列，2 档不该被反复打断
        // 模拟他日志里的呼吸波动：温度来回穿 55 也不该掉档
        let mut lvl = Some(3u8);
        for t in [47.0, 48.0, 46.5, 49.0, 47.5, 48.8, 46.2, 47.2, 46.7] {
            lvl = Some(level_with_hysteresis(lvl, t, 2, 8));
        }
        assert_eq!(lvl, Some(3), "3 档在 45–55 区间内应保持不动");
    }

    #[test]
    fn smoothing_damps_spikes() {
        let mut buf = Vec::new();
        assert!((smooth(&mut buf, 50.0) - 50.0).abs() < 0.01);
        smooth(&mut buf, 70.0); // 尖峰
        let st = smooth(&mut buf, 50.0);
        assert!(st < 60.0, "尖峰应被拉平，实际 {st}");
        assert_eq!(buf.len(), 3, "缓冲只保留 3 个点");
        // 持续高温时平均会跟上
        for _ in 0..5 {
            smooth(&mut buf, 70.0);
        }
        assert!((buf.iter().sum::<f32>() / 3.0 - 70.0).abs() < 0.01);
    }

    #[test]
    fn power_refines_temperature() {
        // 重载（≥12W）：判定温度上浮 5°C → 提前升档
        assert_eq!(effective_temp(50.0, Some(15.0)), 55.0);
        // 空闲（≤4W）：判定温度下调 3°C → 更难升档
        assert_eq!(effective_temp(50.0, Some(2.0)), 47.0);
        // 中载：不修正
        assert_eq!(effective_temp(50.0, Some(8.0)), 50.0);
        // 没有功耗数据：不修正
        assert_eq!(effective_temp(50.0, None), 50.0);
    }

    #[test]
    fn power_window_smooths_spikes() {
        // 天毅的要求：功耗判断必须基于窗口均值，单点尖峰不能带偏
        let mut buf = Vec::new();
        for _ in 0..4 {
            smooth_into(&mut buf, 2.0, 5);
        }
        smooth_into(&mut buf, 25.0, 5); // 单点尖峰
        let avg = smooth_into(&mut buf, 2.0, 5);
        assert!(avg < 10.0, "5 秒窗口稀释了尖峰，实际 {avg}");
        assert!(avg < POWER_HIGH, "尖峰稀释后不应跨入重载阈值 {avg}");
        // 尖峰滑出窗口后均值完全恢复
        for _ in 0..5 {
            smooth_into(&mut buf, 2.0, 5);
        }
        let recovered = smooth_into(&mut buf, 2.0, 5);
        assert!((recovered - 2.0).abs() < 0.01, "窗口滑出后应恢复，实际 {recovered}");
    }

    #[test]
    fn cpu_temp_mapping() {
        // 8 档细映射：每 5°C 一档
        assert_eq!(level_for_cpu_temp(40.0, 1, 8), 1);
        assert_eq!(level_for_cpu_temp(47.0, 1, 8), 2);
        assert_eq!(level_for_cpu_temp(52.0, 1, 8), 3);
        assert_eq!(level_for_cpu_temp(57.0, 1, 8), 4);
        assert_eq!(!(level_for_cpu_temp(62.0, 1, 8) == 5), false);
        assert_eq!(level_for_cpu_temp(67.0, 1, 8), 6);
        assert_eq!(level_for_cpu_temp(72.0, 1, 8), 7);
        assert_eq!(level_for_cpu_temp(85.0, 1, 8), 8);
        assert_eq!(level_for_cpu_temp(40.0, 3, 8), 3); // 下限夹取
        assert_eq!(level_for_cpu_temp(85.0, 1, 6), 6); // 上限夹取
    }

    #[test]
    fn macmon_json_parse() {
        let line = r#"{"temp":{"cpu_temp_avg":47.17,"gpu_temp_avg":37.3},"cpu_power":4.08,"sys_power":10.13}"#;
        assert!((json_f32(line, "cpu_temp_avg").unwrap() - 47.17).abs() < 0.01);
        assert!((json_f32(line, "cpu_power").unwrap() - 4.08).abs() < 0.01);
        assert!((json_f32(line, "sys_power").unwrap() - 10.13).abs() < 0.01);
        assert!(json_f32(line, "not_there").is_none());
        // null 值不能被当成 0
        assert!(json_f32(r#"{"cpu_temp_avg":null}"#, "cpu_temp_avg").is_none());
    }


}

/* ===================== 状态栏模式 ===================== */
// macOS 原生 NSStatusItem：状态栏常驻显示温度和档位，点开菜单直接操作。
// 架构与 TUI 相同：BLE worker 独占设备，UI 只发命令、只读共享状态。
// AppKit 一切操作都在主线程（NSApplication runloop + NSTimer 轮询共享状态）。

use objc2::declare_class;
use objc2::mutability::Immutable;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::DeclaredClass;
use objc2::{sel, ClassType};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
};
use objc2_foundation::{MainThreadMarker, NSString, NSTimer};
use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc;

thread_local! {
    static STATUS_ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
    static MB_MENU: RefCell<Option<Retained<NSMenu>>> = const { RefCell::new(None) };
    static MB_TARGET: RefCell<Option<Retained<MenuTarget>>> = const { RefCell::new(None) };
    static MB_SUB_LIMIT: RefCell<Option<Retained<NSMenu>>> = const { RefCell::new(None) };
}

static CMD_TX_MB: OnceLock<tokio::sync::mpsc::Sender<BleCmd>> = OnceLock::new();
static SHARED_MB: OnceLock<Arc<Mutex<Ui>>> = OnceLock::new();

declare_class!(
    struct MenuTarget;

    unsafe impl ClassType for MenuTarget {
        type Super = NSObject;

        type Mutability = Immutable;

        const NAME: &'static str = "RMCMenuTarget";
    }

    impl DeclaredClass for MenuTarget {}

    unsafe impl NSObjectProtocol for MenuTarget {}

    unsafe impl MenuTarget {
        #[method(menuAction:)]
        unsafe fn menu_action(&self, sender: Option<&NSMenuItem>) {
            let Some(sender) = sender else { return };
            let tag = sender.tag();
            let Some(tx) = CMD_TX_MB.get() else { return };
            let Some(state) = SHARED_MB.get() else { return };
            let mut ui = state.lock().unwrap();

            match tag {
                1..=8 => {
                    let _ = tx.try_send(BleCmd::Fan(tag as u8));
                    ui.level = Some(tag as u8);
                    ui.auto_last = None;
                    if ui.autopilot {
                        ui.manual_until = Some(Instant::now() + Duration::from_secs(60));
                        ui.log(format!("→ 第 {tag} 档（手动接管 60 秒）"));
                    } else {
                        ui.log(format!("→ 第 {tag} 档"));
                    }
                }
                20 => {
                    let want = ui.hall != Some(true);
                    let _ = tx.try_send(BleCmd::Hall(want));
                    ui.hall = Some(want);
                    ui.log(if want { "→ 制冷 开" } else { "→ 制冷 关" });
                }
                21 => {
                    ui.autopilot = !ui.autopilot;
                    if ui.autopilot {
                        if ui.diablo == Some(true) {
                            let _ = tx.try_send(BleCmd::Diablo(false));
                            ui.diablo = Some(false);
                        }
                        if ui.auto_mode == Some(true) {
                            let _ = tx.try_send(BleCmd::Auto(false));
                            ui.auto_mode = Some(false);
                        }
                        ui.manual_until = None;
                        ui.auto_last = None;
                        ui.log("自动调档 开");
                    } else {
                        ui.log("自动调档 关");
                    }
                }
                22 => {
                    let _ = tx.try_send(BleCmd::Reconnect);
                    ui.connected = false;
                    ui.device = "重连中…".to_string();
                    ui.log("已请求重连");
                }
                30..=37 => {
                    // 自动调档上限：写入配置，重启保留
                    let lv = (tag - 29) as u8;
                    ui.auto_max = lv;
                    ui.auto_last = None;
                    config_set("auto_max", &lv.to_string());
                    ui.log(format!("自动调档上限 → {lv} 档（已保存）"));
                }
                99 => std::process::exit(0),
                _ => {}
            }
        }

        #[method(timerTick:)]
        unsafe fn timer_tick(&self, _timer: Option<&NSTimer>) {
            let Some(state) = SHARED_MB.get() else { return };
            let ui = state.lock().unwrap();

            // 状态栏标题：只显示档位（温度在菜单里看）
            let title = if ui.connected {
                format!(
                    "{}档",
                    ui.level
                        .map(|l| l.to_string())
                        .unwrap_or_else(|| "?".into())
                )
            } else {
                "重连中…".to_string()
            };
            #[allow(deprecated)]
            STATUS_ITEM.with_borrow_mut(|slot| {
                if let Some(item) = slot {
                    unsafe { item.setTitle(Some(&*NSString::from_str(&title))) };
                }
            });

            // 菜单勾选状态
            MB_SUB_LIMIT.with_borrow_mut(|slot| {
                if let Some(sub) = slot {
                    for item in sub.itemArray().iter() {
                        let tag = item.tag();
                        if (30..=37).contains(&tag) {
                            let state: i64 =
                                if ui.auto_max == (tag - 29) as u8 { 1 } else { 0 };
                            let _: () = unsafe { objc2::msg_send![item, setState: state] };
                        }
                    }
                }
            });
            MB_MENU.with_borrow_mut(|slot| {
                if let Some(menu) = slot {
                    let items = menu.itemArray();
                    for item in items.iter() {
                        let tag = item.tag();
                        let on = match tag {
                            1..=8 => ui.level == Some(tag as u8),
                            20 => ui.hall == Some(true),
                            21 => ui.autopilot,
                            30..=37 => ui.auto_max == (tag - 29) as u8,
                            _ => continue,
                        };
                        let state: i64 = if on { 1 } else { 0 };
                        let _: () = unsafe { objc2::msg_send![item, setState: state] };
                    }
                }
            });
        }
    }
);

unsafe fn setup_menubar(mtm: MainThreadMarker) -> Retained<NSStatusItem> {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let bar = unsafe { NSStatusBar::systemStatusBar() };
    let item = bar.statusItemWithLength(-1.0);
    #[allow(deprecated)]
    unsafe { item.setTitle(Some(&*NSString::from_str("…"))) };

    let target: Retained<MenuTarget> = unsafe { objc2::msg_send_id![MenuTarget::class(), new] };
    MB_TARGET.with_borrow_mut(|slot| *slot = Some(target.clone()));

    let menu = NSMenu::new(mtm);

    // 状态行：不可点击，timer 每秒刷新文字
    let info = NSMenuItem::new(mtm);
    unsafe { info.setTitle(&NSString::from_str("…")) };
    info.setTag(-1isize);
    menu.addItem(&info);

    let sep1 = NSMenuItem::separatorItem(mtm);
    menu.addItem(&sep1);

    for l in 1..=8u8 {
        let mi = NSMenuItem::new(mtm);
        mi.setTag(l as isize);
        unsafe { mi.setTitle(&NSString::from_str(&format!("第 {l} 档"))) };
        unsafe { mi.setAction(Some(sel!(menuAction:))) };
        mi.setTarget(Some(&target));
        menu.addItem(&mi);
    }

    let sep2 = NSMenuItem::separatorItem(mtm);
    menu.addItem(&sep2);

    let mk = |tag: i32, title: &str| {
        let mi = NSMenuItem::new(mtm);
        mi.setTag(tag as isize);
        unsafe { mi.setTitle(&NSString::from_str(title)) };
        unsafe { mi.setAction(Some(sel!(menuAction:))) };
        mi.setTarget(Some(&target));
        menu.addItem(&mi);
        mi
    };
    mk(20, "制冷开/关");
    mk(21, "自动调档");
    mk(22, "手动重连");

    // 自动上限子菜单：点选即写入配置
    let limit_parent = NSMenuItem::new(mtm);
    unsafe { limit_parent.setTitle(&NSString::from_str("自动上限")) };
    let sub = NSMenu::new(mtm);
    for l in 1..=8u8 {
        let mi = NSMenuItem::new(mtm);
        mi.setTag(29 + l as isize);
        unsafe { mi.setTitle(&NSString::from_str(&format!("最高 {l} 档"))) };
        unsafe { mi.setAction(Some(sel!(menuAction:))) };
        mi.setTarget(Some(&target));
        sub.addItem(&mi);
    }
    limit_parent.setSubmenu(Some(&sub));
    menu.addItem(&limit_parent);
    MB_SUB_LIMIT.with_borrow_mut(|slot| *slot = Some(sub));

    let sep3 = NSMenuItem::separatorItem(mtm);
    menu.addItem(&sep3);
    mk(99, "退出");

    unsafe { item.setMenu(Some(&menu)) };

    MB_MENU.with_borrow_mut(|slot| *slot = Some(menu));

    // 每秒从共享状态刷新标题与勾选
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            1.0,
            &target,
            sel!(timerTick:),
            None,
            true,
        )
    };
    std::mem::forget(timer);

    item
}

pub async fn menubar_main(detached: bool) -> R<()> {
    if !detached {
        eprintln!("正在扫描并连接散热器…");
    }
    let first = open().await?;

    let (cmd_tx, cmd_rx) = mpsc::channel::<BleCmd>(8);
    let (msg_tx, mut msg_rx) = mpsc::channel::<BleMsg>(32);

    let shared = Arc::new(Mutex::new(Ui::default()));
    {
        let mut ui = shared.lock().unwrap();
        ui.auto_max = load_auto_max();
        ui.autopilot = true; // 连上即巡航
        ui.connected = true;
        ui.device = device_label(&first).await;
    }

    CMD_TX_MB.set(cmd_tx.clone()).ok();
    SHARED_MB.set(shared.clone()).ok();

    tokio::spawn(ble_worker(Some(first), cmd_rx, msg_tx.clone()));

    // 后台循环：消息处理、macmon 采样、自动调档决策、历史日志
    let shared_bg = shared.clone();
    tokio::spawn(async move {
        let mut macmon = MacmonFeed::start();
        let mut history = History::new(10);
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                msg = msg_rx.recv() => {
                    if let Some(m) = msg {
                        let mut ui = shared_bg.lock().unwrap();
                        apply_msg(m, &mut ui);
                    }
                }
                sample = next_macmon(&mut macmon) => {
                    if let Some((t, cpu_p, _)) = sample {
                        let mut ui = shared_bg.lock().unwrap();
                        ui.cpu_temp = Some(t);
                        ui.cpu_power = Some(cpu_p);
                    }
                }
                _ = ticker.tick() => {
                    let mut ui = shared_bg.lock().unwrap();
                    ui.heat = thermal_state();
                    ui.locked = screen_locked();
                    ui.idle = idle_seconds();
                    history.maybe_write(&ui);

                    if ui.autopilot && ui.diablo == Some(true) {
                        let _ = cmd_tx.try_send(BleCmd::Diablo(false));
                        ui.diablo = Some(false);
                    }
                    if ui.autopilot && ui.auto_mode == Some(true) {
                        let _ = cmd_tx.try_send(BleCmd::Auto(false));
                        ui.auto_mode = Some(false);
                    }

                    if ui.autopilot {
                        let away = ui.locked || ui.idle >= 300;
                        if away {
                            if ui.hall == Some(true) {
                                let _ = cmd_tx.try_send(BleCmd::Hall(false));
                                ui.hall = Some(false);
                            }
                        } else {
                            if ui.hall != Some(true) {
                                let _ = cmd_tx.try_send(BleCmd::Hall(true));
                                ui.hall = Some(true);
                            }

                            let manual = ui
                                .manual_until
                                .map(|t| Instant::now() < t)
                                .unwrap_or(false);
                            if !manual && ui.manual_until.is_some() {
                                ui.manual_until = None;
                            }

                            if !manual {
                                let cap = match ui.limit {
                                    Some(8) | None => 8,
                                    Some(l) if (1..=8).contains(&l) => l,
                                    Some(_) => 8,
                                };
                                let eff_max = ui.auto_max.min(cap);
                                // 功耗用 5 秒窗口均值：单点尖峰会把档位带着飘
                                let power_avg =
                                    ui.cpu_power.map(|p| smooth_into(&mut ui.power_buf, p, POWER_WINDOW));
                                let (candidate, why) = if let Some(raw_t) = ui.cpu_temp {
                                    let st = smooth(&mut ui.temp_buf, raw_t);
                                    let eff = effective_temp(st, power_avg);
                                    (
                                        level_with_hysteresis(ui.level, eff, 2, eff_max),
                                        format!(
                                            "CPU {st:.1}°C · {:.1}W",
                                            power_avg.unwrap_or(0.0)
                                        ),
                                    )
                                } else {
                                    (
                                        level_for_heat(ui.heat, 2, eff_max),
                                        format!("热压力{}", heat_name(ui.heat)),
                                    )
                                };

                                let cur = ui.level.unwrap_or(0);
                                if candidate > cur {
                                    let _ = cmd_tx.try_send(BleCmd::Fan(candidate));
                                    ui.level = Some(candidate);
                                    ui.down_votes = 0;
                                    ui.log(format!("自动：{why} → 第 {candidate} 档"));
                                } else if candidate < cur && cur > 0 {
                                    // 功耗还在高位就不投降档票：温度低只是散热太好，
                                    // 降了档 CPU 马上又要热回去
                                    let power_ok =
                                        power_avg.map(|p| p <= POWER_LOW).unwrap_or(true);
                                    if power_ok {
                                        ui.down_votes += 1;
                                        if ui.down_votes >= DOWN_CONFIRM_TICKS {
                                            let _ = cmd_tx.try_send(BleCmd::Fan(candidate));
                                            ui.level = Some(candidate);
                                            ui.log(format!(
                                                "自动：{why} → 第 {candidate} 档（持续 {} 秒低温低载已确认）",
                                                DOWN_CONFIRM_TICKS
                                            ));
                                            ui.down_votes = 0;
                                        }
                                    } else {
                                        ui.down_votes = 0;
                                    }
                                } else {
                                    ui.down_votes = 0;
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    // 主线程：AppKit runloop（阻塞直到退出）
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let status = unsafe { setup_menubar(mtm) };
    STATUS_ITEM.with_borrow_mut(|slot| *slot = Some(status));

    unsafe {
        let app = NSApplication::sharedApplication(mtm);
        app.run();
    }
    Ok(())
}


#[allow(dead_code)]
const _: () = {
    fn assert_msg<T: objc2::Message>() {}
    fn _f() {
        assert_msg::<MenuTarget>();
    }
};
