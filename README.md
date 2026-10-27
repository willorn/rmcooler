# rmcooler

macOS 状态栏控制 **红魔散热器 8 Pro（RM Magcooler 8pro）** 的小工具。

菜单栏常驻显示散热器温度与当前档位，点开即可调档、开关制冷、切换自动巡航——不用开手机 App。

> 非官方工具。BLE 协议为社区逆向成果，与努比亚官方无关。

## 特性

- **状态栏常驻**：实时显示制冷片温度与风扇档位，断连自动重连
- **自动调档**：跟随 CPU 真实温度 + 功耗（macmon 提供免 sudo 数据），带施密特迟滞防抖
  - 重载（功耗均值 ≥ 12W）判定温度上浮，提前升档
  - 降档需连续 60 秒「低温 + 低载」双确认，宁高勿低
- **离开检测**：锁屏 / 屏保 / 空闲 5 分钟自动关制冷
- **历史日志**：每 10 秒写入 `~/.rmcooler/history-YYYY-MM-DD.csv`
- **数据摘要**：`hm report` 一条命令看全天温度/功耗/档位分布
- **供电限档感知**：充电器功率不足时（设备上报限档）自动不冲击高档

## 要求

- Apple Silicon Mac（M1–M5），macOS 11+
- 红魔散热器 8 Pro
- [macmon](https://github.com/vladkens/macmon)（`brew install macmon`，可选——提供免 sudo 的真实 CPU 温度/功耗，未安装时降级为热压力信号）

## 构建

```sh
cargo build --release
```

## 安装

```sh
# 软链到 PATH（状态栏 .app 与脚本依赖它）
ln -sf "$(pwd)/target/release/rmcooler" ~/bin/rmcooler

# 状态栏常驻
hm   # 或 ./target/release/rmcooler
```

双击 `RedMagicCooler.app`（由 `cooler.applescript` 编译）也可以弹菜单操作：

```sh
osacompile -o RedMagicCooler.app cooler.applescript
```

## 使用

| 命令 | 作用 |
|---|---|
| `hm` | 状态栏模式（后台化启动，终端可关） |
| `hm off` / `hm on` | 关制冷 / 开制冷 + 6 档 |
| `hm level 5` | 指定档位 |
| `hm boost on\|off` | 破坏神模式 |
| `hm status` | 读一次状态 |
| `hm report [YYYY-MM-DD]` | 当日摘要 |
| `hm daemon` | 后台自动调档（无界面） |
| `hm service install` | 开机自启（LaunchAgent） |

状态栏菜单：1–8 档、制冷开关、自动调档开关、手动重连、退出。

## 自动调档策略

- 判定温度 = CPU 温度（3 秒均值）+ 功耗修正（5 秒均值：重载 +5°C / 空闲 −3°C）
- 施密特迟滞 ±5°C：升档即时，降档需连续 60 秒「低温 + 低载」双确认
- 档位映射每 5°C 一档（1–8 连续），受供电限档硬约束

## 协议要点（逆向）

- 主服务 `d52082ad-e805-9f97-9d4e-1c682d9c9ce6`
- 档位写入 `1012`（raw 值非线性：`2e 34 3a 40 44 48 4c 50`）
- 状态通知 `1015`：`04 xx` 温度（**显示值 = xx − 6**）、`07 xx` 供电限档（8 = 不限）、`08 hi lo` 大端转速、`09 xx` 功率
- 制冷开关 `1011`：`02` 开 / `03` 关；破坏神 `1017`；设备温控 `1018`

> 注意：设备同一时间只接受一个 BLE 中心；供电不足时设备会上报限档并自行压制功率。

## License

[MIT](LICENSE)
