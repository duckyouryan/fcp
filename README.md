# fcp

Fan control for motherboards that only have "slow" and "fast".

`fcp` lets you set any fan speed from 0 to 100%, or let the CPU temperature
decide, on boards where the normal Linux fan tools do nothing. It is one small
program with no dependencies.

Written for, and used on, a Lenovo ThinkCentre M79 (ITE it8792 fan controller,
AMD CPU). It should help on any board with the same symptoms, but that is untested.

## The problem

On some boards you cannot choose a fan speed. You write a number into the
fan's `pwm` file and nothing happens. The only thing the fan controller reacts
to is its mode switch, `pwmN_enable`:

- mode `0`: full speed;
- mode `1` and `2`: "automatic". The board decides the speed from a temperature
  sensor of its own choosing. On the M79 that sensor is not the CPU, so under
  load the fans can stay slow while the CPU gets hot.

## What fcp does

It flips the mode switch between slowest and fastest, 10 times a second. A fan
cannot speed up and slow down that fast, so it settles at a speed in between.

- You ask for **25%**: every 100 ms, the fan gets 25 ms of "fast" and 75 ms of "slow".
- You ask for **0%**: always "slow". **100%**: always "fast". No flipping.

On top of that, a fan can follow a **temperature curve**. With `curve 40:0 75:100`:

| CPU temperature | Fan speed |
|---|---|
| 40 °C or less | 0% (the slowest the board allows) |
| about 57 °C | about 50% |
| 75 °C or more | 100% |
| sensor cannot be read | 100% until it can |

## Is this for you?

Only if writing a speed does nothing on your machine. Check (replace `N` with
your hwmon number, see "Find your paths" below), and **write the original values
back afterwards**:

```sh
H=/sys/class/hwmon/hwmonN
cat $H/pwm3_enable                                  # remember this value
echo 1  | sudo tee $H/pwm3_enable
echo 40 | sudo tee $H/pwm3                          # ask for a low speed
sleep 10; cat $H/fan3_input                         # rpm
echo 255 | sudo tee $H/pwm3
sleep 10; cat $H/fan3_input                         # rpm again
```

If the two rpm numbers differ, your board works normally and you do not need fcp.
If they are the same, you do.

## Install

You need Linux with systemd, root access and Rust 1.70 or newer.

```sh
cargo build --release
sudo install -Dm755 target/release/fcp /usr/local/bin/fcp
sudo install -Dm644 fcp.service /etc/systemd/system/fcp.service
sudo install -Dm644 fcp.conf /etc/fcp.conf
```

Edit `/etc/fcp.conf` (next section), then:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now fcp
fcp fan list
```

On the first start every fan is in `system` mode: fcp changes nothing until you tell it to.

## Configure

`/etc/fcp.conf` is a plain text file. `#` starts a comment.

```
frequency 10                 # flips per second

fan CPU_FAN
    pwm    /sys/class/hwmon/hwmon4/pwm3_enable chip=it8792 min=2 max=0
    tach   /sys/class/hwmon/hwmon4/fan3_input  chip=it8792
    sensor /sys/class/hwmon/hwmon3/temp1_input chip=k10temp
    curve  40:0 75:100
```

| Line | Meaning |
|---|---|
| `frequency` | Flips per second. Default 10, allowed 0.1 to 1000. Higher is smoother for the fan but means more switching. Very short phases get inexact because of OS timer jitter, so raise it only if you can hear the fan pulsing. |
| `fan NAME` | Starts a fan. The name is what you type in `fcp fan NAME ...`. |
| `pwm` | The mode-switch file to write. `min=` is the value that means slowest, `max=` the value that means fastest. |
| `tach` | The file with the fan's rpm. Optional, only for `fcp fan list`. |
| `sensor` | The file with the temperature, in thousandths of °C (what Linux gives you). Needed for `auto`. |
| `curve` | Points `TEMP:PERCENT`, in rising temperature order. Straight lines between them, flat outside. Needed for `auto`. |
| `chip=NAME` | Optional safety. fcp reads `NAME` from the `name` file next to the path and refuses to start if it differs. |

A fan with only `tach` (no `pwm`) is shown in the list but cannot be controlled.

### Find your paths

```sh
grep -H . /sys/class/hwmon/hwmon*/name    # which hwmonN is which chip
ls /sys/class/hwmon/hwmonN/               # pwm*_enable, fan*_input, temp*_input
```

- **Which pwm is which fan:** write `0` to a `pwmN_enable` and watch which
  `fanN_input` rpm goes up. Then put the old value back.
- **Which value is slowest:** try the values the file accepts (usually 0, 1, 2).
  `0` is normally full speed. The one with the lowest rpm is your `min`.
- **Which sensor is the CPU:** load the CPU
  (`for i in $(seq $(nproc)); do timeout 30 sh -c 'while :; do :; done' & done`)
  and see which `temp*_input` climbs with it. On AMD, `k10temp` is the CPU itself.

The hwmon numbers can change after a reboot. With `chip=` in place, fcp then
refuses to start and tells you which path is wrong, instead of silently driving
the wrong fan.

## Use

```
$ fcp fan list
FAN      MODE     SET      SPEED    TEMP
CPU_FAN  auto     50%    730 rpm   58 °C
SYS_FAN  system     -   1100 rpm   58 °C

$ fcp fan CPU_FAN 40       # manual, 40%
$ fcp fan CPU_FAN auto     # follow the curve
$ fcp fan CPU_FAN system   # hand the fan back
$ fcp config-path          # where the config is
```

No `sudo` needed for these commands.

Each fan is in one of three modes:

- **system**: fcp does not touch the fan. The board does what it always did.
- **manual**: a fixed percent that you chose.
- **auto**: the percent follows the curve, re-checked every second.

When a fan goes from `system` to `manual` or `auto`, fcp remembers the value
the board had set. When it goes back to `system`, that value is written back.

## What happens when

| Situation | Result |
|---|---|
| `fcp fan X system` | X goes back to exactly what the board had before fcp. |
| `systemctl stop fcp` | Same for all fans. The modes are remembered, so the next start continues where you left off. |
| Reboot | The board starts in its own default. At boot fcp resumes the remembered modes. |
| Sensor cannot be read | That fan runs at 100% until the sensor works again. A line goes to the log. |
| fcp is killed hard (`kill -9`, crash) | The fans stay at whatever they were doing, possibly slow. systemd restarts fcp after 2 seconds. `fcp fan X system` then still gives back the right value. |
| fcp is not running | You cannot control the fans. After a normal stop they are back to the board's own behaviour. |

## Files

| Path | What |
|---|---|
| `/etc/fcp.conf` | Config. |
| `/var/lib/fcp/state` | Modes and remembered board values. Written by fcp, not by hand. |
| `/run/fcp.sock` | The control socket. |

The environment variables `FCP_CONFIG`, `FCP_STATE` and `FCP_SOCKET` override these.
Logs: `journalctl -u fcp`.

## Troubleshooting

- **`cannot connect to /run/fcp.sock`**: the daemon is not running. `systemctl status fcp`.
- **fcp does not start, says a path belongs to another chip**: the hwmon numbers shifted. Fix the paths in `/etc/fcp.conf`.
- **The fan pulses audibly at low percent**: raise `frequency`, for example to 25.
- **The fan hunts up and down around one temperature**: the curve is plain, with no smoothing. Make it less steep.
- **Another tool (`fancontrol`, `thinkfan`, ...) is also running**: stop it. Two programs driving the same fan will fight.

## Good to know

- Anyone with a login on the machine can change fan speeds. The control socket is
  open to all local users on purpose, so that no `sudo` is needed.
- fcp does not guard against a stuck sensor that always reports a cool value.

## Uninstall

```sh
sudo systemctl disable --now fcp      # gives the fans back to the board
sudo rm /usr/local/bin/fcp /etc/systemd/system/fcp.service /etc/fcp.conf
sudo rm -r /var/lib/fcp
```

## Development

```sh
cargo test
```
