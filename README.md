# gigabyte-u4-charge

[Русская версия (README.ru.md)](README.ru.md) | [Hardware & EC Notes](docs/HARDWARE_NOTES.md) | [Upstream & Distribution Guide](docs/DISTRIBUTION.md)

Hardware battery charge threshold support and a pure safe Rust management daemon for **GIGABYTE U4 UD (`RC14UD`) / Clevo `L14xMU`** laptops running Linux (verified on Arch Linux `7.2.14-arch1-1`, EC `7.7`, BIOS `F4`).

---

## Why This Exists

1. **`gigabyte-laptop-wmi` does not support U-series notebooks.** Unlike AERO/AORUS laptops (`WMBC`/`WMBD`), the GIGABYTE U4 uses a Clevo `L14xMU` ODM motherboard (`CLV0001` ACPI + ITE IT5570E EC).
2. **Upstream `tuxedo-drivers` / `clevo-drivers` fails on `RC14UD` / `L14xMU`:**
   - `L14xMU` is excluded in `clevo_has_legacy_flexicharger()`.
   - Even if forced on, `ZEVT(0x76)` subcommand `0x05` sets EC mailbox byte `FBF1=0x01`, which IT5570E v7.7 ignores while charging continues to 100%.
   - ACPI `_DSM(0x04)` (`PEVT`, used upstream by `clevo_fn_lock_available()` / `clevo_has_cc4_flexicharger()`) hits a DSDT path missing `Release(PATM)`.
   - Once thresholds stop charging, raising them without resetting `FBF1=0x00` leaves the EC latched in `Not charging`.

## How It Works

This repository combines two clean layers:

1. **Kernel / DKMS Layer ([`kernel/`](kernel/))** — Patches `clevo_acpi.c` & `clevo_keyboard.h` (`tuxedo-drivers` v4.25.0):
   - Exposes native Linux sysfs thresholds under `/sys/class/power_supply/BAT0/charge_control_{start,end}_threshold` and `charge_type` (`Custom` / `Standard`).
   - Uses atomic `ZEVT(0x41)` `0xCB` writes (`FBF1=0x02` for Custom thresholds, `0x642800CB` for Standard 100%).
   - Performs an automatic `0x642800CB` unlatch + `250 ms` charger soft-start delay before applying raised Custom thresholds or restoring thresholds on suspend resume.
   - Serializes EC mailbox transactions via `clevo_acpi_ec_lock` (`120 ms` settle outside AML `PATM`), eliminating races with `tccd` polling (`0x63`/`0x64`).
   - Also provides a minimal upstream-ready patch ([`kernel/upstream-tuxedo-drivers-l14xmu.patch`](kernel/upstream-tuxedo-drivers-l14xmu.patch)) for submission to `tuxedo-drivers`.

2. **Userspace Daemon & CLI ([`daemon/`](daemon/) + [`systemd/`](systemd/))** — Pure safe Rust (`#![forbid(unsafe_code)]`, `#![deny(clippy::unwrap_used)]`, zero external crates):
   - Persists custom limits across reboots and suspend/resume (wall vs monotonic clock skew detection with backwards NTP step guard).
   - Coexists with **KDE Plasma 6 PowerDevil** and **TLP**: manual threshold changes in `Custom` mode are adopted cleanly instead of overwritten.
   - **No hidden 100% charging:** auto-balancing is disabled by default (`BALANCE_INTERVAL_DAYS=0`), and `full` / `balance` are strictly one-shot with auto-revert on full charge, AC unplug, reboot, or safety timeout.
   - Unprivileged Unix socket (`/run/u4-charge/control.sock`, `root:wheel` `0660`, `500 ms` timeouts, `64 B` max command) plus `u4-charge notify` for interactive KDE desktop notifications (`notify-send`).

---

## Supported Thresholds (EC IT5570E Hardware Limits)

- `START_THRESHOLD`: `40`, `50`, `60`, `70`, `80`, `95`
- `END_THRESHOLD`: `60`, `70`, `80`, `90`, `100` (`START < END`)

---

## Installation (Arch Linux)

### Prerequisites
- `base-devel`, `dkms`, `linux-headers`, `rust` (`cargo`), `libnotify` (`notify-send`, optional for KDE popup)
- User account in group `wheel` (default on Arch) for unprivileged CLI/notification control via `/run/u4-charge/control.sock`

### Build & Install

```bash
git clone https://github.com/Vim-01/gigabyte-u4-charge.git
cd gigabyte-u4-charge
sudo ./install.sh
```

What `install.sh` does:
1. Builds `u4-charge` in `--release` mode.
2. Builds the Arch DKMS package `clevo-drivers-u4-dkms` via `makepkg` (replacing `clevo-drivers-dkms-git` / `tuxedo-drivers-dkms` while preserving `provides=(tuxedo-drivers-dkms ...)` for `tuxedo-control-center-bin`).
3. Reloads the kernel modules cleanly in dependency order and verifies `/sys/class/power_supply/BAT0/charge_control_end_threshold`.
4. Installs `/usr/local/bin/u4-charge`, `/etc/u4-charge.conf`, `/etc/systemd/system/u4-charge.service`, and `/etc/systemd/user/u4-charge-notify.service`.

---

## Usage

```bash
# View battery & EC telemetry + daemon mode
u4-charge status

# Change persistent charge limits (routes via daemon socket for wheel users)
u4-charge set --start 60 --end 80

# One-shot full charge to 100% (auto-reverts when full, unplugged, or after FULL_TIMEOUT_HOURS)
u4-charge full

# Cancel active one-shot full/balance and immediately restore configured limit
u4-charge cancel

# One-shot calibration / balance (100% + soak minutes configured in /etc/u4-charge.conf)
u4-charge balance

# Trigger interactive KDE desktop prompt manually
u4-charge notify
```

---

## Configuration (`/etc/u4-charge.conf`)

```ini
START_THRESHOLD=60
END_THRESHOLD=80

# Automatic calibration prompt disabled by default (0)
BALANCE_INTERVAL_DAYS=0
BALANCE_SOAK_MINUTES=90

# Safety timeout for one-shot Full mode (hours)
FULL_TIMEOUT_HOURS=6

# Show interactive KDE toast on session start asking whether to charge once to 100%
NOTIFY_ON_START=true
```

---

## Standard Sysfs Usage (Without the Daemon / With TLP or KDE PowerDevil)

Once `clevo-drivers-u4-dkms` is loaded, native tools work directly:

```bash
echo 60 | sudo tee /sys/class/power_supply/BAT0/charge_control_start_threshold
echo 80 | sudo tee /sys/class/power_supply/BAT0/charge_control_end_threshold
echo Custom | sudo tee /sys/class/power_supply/BAT0/charge_type
```

---

## License

GPL-2.0-or-later (compatible with upstream `tuxedo-drivers`, see [`LICENSE`](LICENSE)).
