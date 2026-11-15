# Hardware & EC Reverse-Engineering Notes: GIGABYTE U4 UD (`RC14UD`) / Clevo `L14xMU`

## 1. Platform Identification

Observed on target system via `/sys/class/dmi/id/` and `/sys/class/power_supply/BAT0/`:

- `sys_vendor`: `GIGABYTE`
- `product_name` / `product_sku`: `RC14UD`
- `board_name`: `U4` (Clevo `L14xMU` ODM barebone family)
- `bios_version`: `F4` (`bios_date`: `05/01/2021`)
- `ec_firmware_release`: `7.7` (ITE IT5570E Embedded Controller)
- Battery: `Notebook BAT` (Design: `9600 mAh`, Full measured: `9523 mAh`)
- Mains sysfs node: `/sys/class/power_supply/AC/online`
- Supported threshold sets (`BAT0/charge_control_*_available_thresholds`):
  - Start: `40 50 60 70 80 95`
  - End: `60 70 80 90 100`
  - Requirement: `start < end`

> **Why `gigabyte-laptop-wmi` does not apply:** Gigabyte AERO/AORUS notebooks use Gigabyte's `WMBC`/`WMBD` WMI methods. Gigabyte U-series (`U4`) laptops use a **Clevo** ACPI/EC design (`CLV0001`), which is why `tangalbert919/gigabyte-laptop-wmi` explicitly lists all U-series models as unsupported.

---

## 2. Root Causes Found in Upstream `tuxedo-drivers` (v4.25.0)

1. **`L14xMU` exclusion & non-working `ZEVT(0x76)` Case `0x05`:**
   - Upstream `clevo_has_legacy_flexicharger()` excluded `L14xMU` in `DMI_BOARD_NAME`.
   - Even when enabled, upstream `clevo_legacy_flexicharger_write()` sent `ZEVT(0x76)` subcommand `0x05`, which sets EC mailbox `FBF1 = 0x01`. On IT5570E EC v7.7, `FBF1 = 0x01` is ignored by firmware and battery continues charging to 100%.

2. **Working Atomic Protocol (`ZEVT(0x41)` with `FCMD = 0xCB`):**
   - `ZEVT(0x41)` executes an atomic mailbox write setting `(FDAT, FBUF, FBF1, FCMD)`:
     - **Custom limit (`FBF1 = 0x02`)**: `arg = (end << 24) | (start << 16) | 0x02CB` (e.g. `0x503C02CB` = 60%–80%).
     - **Standard 100% (`FBF1 = 0x00`)**: `arg = 0x642800CB` (40%–100%, FlexiCharger off).
   - **Empirical threshold verification:**
     - Sending `0x645F02CB` (`95%–100%`) while actively charging at `87%` continued charging (`2554 mA`), confirming IT5570E v7.7 genuinely evaluates numeric start/end comparators (not just a blind inhibit bit).
     - Sending `0x503C02CB` (`60%–80%`) at `87%` cleanly halted charging (`0 mA`, `Not charging`), surviving both `tccd` background polling and 3s physical AC unplug/replug.
     - Single call `0x642800CB` resumed charging (`735 mA`) without needing `ZEVT(0x76)`.

3. **EC Latch Quirk (Requires Unlatch via `0x00CB`):**
   - Once latched in `Not charging`, writing higher Custom thresholds directly with `FBF1 = 0x02` keeps the EC latched in `Not charging`.
   - Switching through `0x642800CB` (`FBF1 = 0x00`), sleeping `250 ms` outside the ACPI mutex (charger soft-start margin), and then writing `0x02CB` reliably unlatches the comparator.
   - Same unlatch sequence is invoked in `clevo_keyboard_resume()` after S3/suspend recovery.

4. **Mailbox Race with `tccd` (`clevo_acpi_ec_lock`):**
   - TUXEDO Control Center (`tccd`) polls ACPI methods `0x63`/`0x64` (`FCMD = 0xC0`) every few seconds via the shared EC mailbox (`0xF8..0xFC`).
   - Patching `clevo_acpi.c` to serialize calls with `clevo_acpi_ec_lock` and sleep `120 ms` after mailbox writes (`0x41`, `0x76`, `0x79`) — after AML `PATM` has already been released — prevents polling collisions without risking AML deadlock.
   - Additionally, `clevo_acpi_evaluate()` passes the 4-byte `u32` argument in an ACPI `BUFFER` object instead of `INTEGER`, matching DSDT `CreateField` expectations.

5. **ACPI `_DSM(0x04)` (`PEVT`) `PATM` Mutex Leak Bug in DSDT:**
   - Calling `_DSM` `Arg2 == 0x04` (used upstream by `clevo_fn_lock_available()` subcommand `0x18` and `clevo_has_cc4_flexicharger()` subcommand `0x1E`) hits a DSDT path where `Acquire(PATM)` can return without `Release(PATM)` on unsupported subcommands. Skipping `_DSM(0x04)` avoids hanging subsequent ACPI EC operations.

6. **BIOS NVRAM Readback (`0x77`) Is Boot-Static:**
   - `ZEVT(0x77)` returns the boot-time BIOS NVRAM snapshot (`BAEE`, `BSCP`, `BTCP`), which DSDT does not update when `0xCB` writes EC RAM. The kernel driver initializes shadow state once from `0x77` and updates it atomically under `clevo_legacy_fc_lock`.

7. **EC RAM Telemetry Offsets Verified (Read-Only `ec_live_state`):**
   Read safely in 1-byte transactions via `ZEVT(0x6B)` + `ZEVT(0x73)` (`500 ms` jiffies cache, `0444`):
   - `0x10`: `ADP` / battery presence bits
   - `0x1A` (u16): `BFC0` (full charge capacity, mAh)
   - `0x26`: `BST0` (bit 0 = discharging, bit 1 = charging)
   - `0x2A` (u16): `BPR0` (battery current, mA)
   - `0x2E` (u16): `BRC0` (remaining capacity, mAh)
   - `0x32` (u16): `BPV0` (pack voltage, mV)
   - `0x38`: `BRS0` (BMS RSOC %)
   - `0xEA`: EC charge flags (`RCHG` bit 5, `ACOT` bit 6)
   - `0xF8..0xFC`: EC mailbox `FCMD`, `FDAT`, `FBUF`, `FBF1`, `FBF2`
