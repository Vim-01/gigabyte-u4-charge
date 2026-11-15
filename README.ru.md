# gigabyte-u4-charge

[English Version (README.md)](README.md) | [Исследование железа и EC](docs/HARDWARE_NOTES.md) | [Куда и как распространять](docs/DISTRIBUTION.md)

Аппаратное управление порогами заряда батареи и безопасный демон на Rust для ноутбуков **GIGABYTE U4 UD (`RC14UD`) / Clevo `L14xMU`** под Linux (проверено на Arch Linux `7.2.14-arch1-1`, EC `7.7`, BIOS `F4`).

---

## В чём проблема штатных драйверов

1. **Драйвер `gigabyte-laptop-wmi` не поддерживает U-серию.** В отличие от игровых моделей AERO/AORUS (`WMBC`/`WMBD`), GIGABYTE U4 построен на ODM-платформе **Clevo `L14xMU`** (`CLV0001` ACPI + контроллер EC ITE IT5570E).
2. **Непатченный `tuxedo-drivers` / `clevo-drivers` не работает на `RC14UD` / `L14xMU`:**
   - Платформа `L14xMU` заблокирована в `clevo_has_legacy_flexicharger()`.
   - Даже при снятии блокировки метод `ZEVT(0x76)` (подкоманда `0x05`) пишет в EC mailbox байт `FBF1=0x01`, который прошивка IT5570E v7.7 игнорирует — батарея продолжает заряжаться до 100%.
   - Вызовы ACPI `_DSM(0x04)` (`PEVT`, которые использует апстрим для FnLock / CC4) на этой DSDT упираются в путь без `Release(PATM)`.
   - После срабатывания лимита EC «защёлкивается» в состоянии `Not charging`: при повышении порога без предварительного сброса через `FBF1=0x00` зарядка не возобновляется.

## Архитектура решения

1. **Уровень ядра / DKMS ([`kernel/`](kernel/))** — патч для `tuxedo-drivers` v4.25.0 (`clevo_acpi.c`, `clevo_keyboard.h`):
   - Открывает нативный интерфейс ядра `/sys/class/power_supply/BAT0/charge_control_{start,end}_threshold` и `charge_type` (`Custom` / `Standard`).
   - Использует атомарную команду `ZEVT(0x41)` (`0x02CB` для `Custom`, `0x642800CB` для `Standard 100%`).
   - При изменении порогов или выходе из сна автоматически снимает защёлку (`0x642800CB` → `msleep(250)` → `0x02CB`).
   - Сериализует обращения к EC Mailbox мьютексом `clevo_acpi_ec_lock` (`msleep(120)` после освобождения ACPI `PATM`), полностью устраняя гонки с фоновым опросом `tccd` (`0x63`/`0x64`).
   - Также включает чистый минимальный патч [`kernel/upstream-tuxedo-drivers-l14xmu.patch`](kernel/upstream-tuxedo-drivers-l14xmu.patch) для отправки Merge Request в апстрим `tuxedo-drivers`.

2. **Демон и CLI [`daemon/`](daemon/) + [`systemd/`](systemd/)** — чистый Safe Rust (`#![forbid(unsafe_code)]`, `#![deny(clippy::unwrap_used)]`, 0 внешних крейтов):
   - Удерживает лимит после перезагрузок и выхода из сна (детектирует suspend/resume по расхождению wall/monotonic часов с защитой от обратного шага NTP).
   - Дружит с **KDE Plasma 6 PowerDevil** и **TLP**: ручное изменение порогов в режиме `Custom` корректно подхватывается демоном.
   - **Никаких скрытых зарядок до 100%:** автобалансировка по умолчанию отключена (`BALANCE_INTERVAL_DAYS=0`), а режимы `full` и `balance` строго разовые (автовозврат к лимиту при 100%, отключении кабеля, перезагрузке или по таймауту).
   - Непривилегированный Unix-сокет `/run/u4-charge/control.sock` (`root:wheel`, права `0660`, тайм-ауты `500 ms`, буфер команд `<= 64 B`) и сервис пользовательской сессии `u4-charge-notify.service` для интерактивных уведомлений KDE.

---

## Допустимые пороги EC (IT5570E)

- `START_THRESHOLD`: `40`, `50`, `60`, `70`, `80`, `95`
- `END_THRESHOLD`: `60`, `70`, `80`, `90`, `100` (строго `START < END`)

---

## Установка (Arch Linux)

### Требования
- `base-devel`, `dkms`, `linux-headers`, `rust` (`cargo`), `libnotify` (`notify-send` для KDE-уведомлений)
- Пользователь в группе `wheel` для управления без `sudo` через сокет `/run/u4-charge/control.sock`

### Сборка и установка

```bash
git clone https://github.com/Vim-01/gigabyte-u4-charge.git
cd gigabyte-u4-charge
sudo ./install.sh
```

Скрипт `install.sh` собирает release-бинарник, строит и ставит DKMS-пакет `clevo-drivers-u4-dkms` (корректно заменяя `clevo-drivers-dkms-git` с сохранением `provides=(tuxedo-drivers-dkms ...)` для `tuxedo-control-center-bin`), перезагружает модули ядра, проверяет sysfs и запускает сервисы systemd.

---

## Команды CLI

```bash
# Текущий статус батареи, режима EC и демона
u4-charge status

# Изменить рабочие пороги (через сокет демона без sudo, если пользователь в wheel)
u4-charge set --start 60 --end 80

# Разовая зарядка до 100% (сама вернёт лимит при достижении 100% или отключении кабеля)
u4-charge full

# Отменить разовый full/balance и сразу вернуть лимит
u4-charge cancel

# Разовая балансировка 100% + выдержка (по явному запросу)
u4-charge balance

# Показать интерактивное уведомление KDE
u4-charge notify
```

---

## Настройка (`/etc/u4-charge.conf`)

```ini
START_THRESHOLD=60
END_THRESHOLD=80

# Автобалансировка отключена по умолчанию (0)
BALANCE_INTERVAL_DAYS=0
BALANCE_SOAK_MINUTES=90

# Таймаут безопасности разового заряда до 100% (в часах)
FULL_TIMEOUT_HOURS=6

# Уведомление при запуске сессии KDE с предложением разового заряда до 100%
NOTIFY_ON_START=true
```

---

## Лицензия

GPL-2.0-or-later (см. [`LICENSE`](LICENSE)).
