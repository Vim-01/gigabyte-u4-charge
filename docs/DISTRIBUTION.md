# Куда и как распространять решение (Distribution Plan)

Ниже зафиксирована проверенная стратегия публикации решения для владельцев GIGABYTE U4 UD (`RC14UD` / `U4` / Clevo `L14xMU`, EC `7.7` на базе ITE IT5570E).

---

## 1. Почему отдельный репозиторий (`Vim-01/gigabyte-u4-charge`) — основа

1. **Самостоятельный пакет «из коробки»:** объединяет ядровой DKMS-патч (`clevo-drivers-u4-dkms`), безопасный пользовательский демон и CLI `u4-charge` (Rust без `unsafe`), systemd-юниты (`u4-charge.service`, `u4-charge-notify.service`) и документацию на EN/RU.
2. **Нейтральная база для ссылок:** в сторонних проектах лучше ссылаться на конкретный технический патч и проверенные замеры в этом репозитории, а не оставлять «маркетинговые» комментарии без контекста.

---

## 2. Релевантные каналы (после согласования с пользователем)

### Канал 1: Собственный GitHub-репозиторий (`Vim-01/gigabyte-u4-charge`)
- **Статус:** опубликован (`main`).
- **Что содержит:** `README.md`, `README.ru.md`, `docs/HARDWARE_NOTES.md`, минимальный DMI-ограниченный патч [`kernel/upstream-tuxedo-drivers-l14xmu.patch`](../kernel/upstream-tuxedo-drivers-l14xmu.patch), `kernel/PKGBUILD`, `daemon/`, `systemd/`, `install.sh`.

### Канал 2: Форк для Clevo / Arch — `nick42d/clevo-drivers` (GitHub RFC Issue)
- **Куда:** [https://github.com/nick42d/clevo-drivers/issues/new](https://github.com/nick42d/clevo-drivers/issues/new) (апстрим AUR-пакета `clevo-drivers-dkms-git`).
- **Формат:** **RFC Issue / технический отчёт**, а не слепой Pull Request с 300+ строками C-кода. Причина: `nick42d/clevo-drivers` — это автоматизированный AUR-пайплайн (`just update-to-upstream` + `just push-to-aur`), который копирует только `PKGBUILD`, `.SRCINFO`, `dkms.conf` и минимальный `patch.diff` (сейчас — 18 строк для отключения `tuxedo_compatibility_check`).
- **Что приложить (Option A):** ссылку на минимальный, строго ограниченный по DMI (`DMI_SYS_VENDOR=GIGABYTE` + `RC14UD`/`U4`/`L14xMU`) патч [`kernel/upstream-tuxedo-drivers-l14xmu.patch`](../kernel/upstream-tuxedo-drivers-l14xmu.patch), чтобы не затронуть другие модели Clevo/System76/Schenker, плюс вопрос мейнтейнеру: готов ли он принять второй патч в `PKGBUILD` или предпочитает отдельный пакет.
- **Опционально — AUR:** при желании позже можно опубликовать отдельный пакет `clevo-drivers-u4-dkms` / `u4-charge` в AUR.

---

## 3. Каналы, которые осознанно пропускаются (чтобы избежать оффтопа и лишних отказов)

1. **TUXEDO Computers GitLab (`tuxedo-drivers`) / support mail:** апстрим TUXEDO официально не поддерживает ноутбуки сторонних производителей (включая GIGABYTE) и требует аккаунт GitLab.
2. **`linrunner/TLP` (#803):** TLP 1.9+ не содержит кода протокола `clevo_acpi` — он автоматически работает поверх стандартного `/sys/class/power_supply/BAT0/charge_control_*_threshold`, как только его экспортирует ядерный драйвер. Писать туда без апстрим-коммита ядра не имеет смысла и выглядит как оффтоп/самореклама.
3. **`tangalbert919/gigabyte-laptop-wmi`:** автор явно отметил U-серию как неподдерживаемую (у U4 архитектура Clevo ODM, а не WMI `WMBC`/`WMBD`). Открывать там Issue со ссылкой на сторонний репозиторий также выглядит как нежелательная реклама.
4. **Сторонние форумы / соцсети (Reddit, Discord, Habr и др.):** не используются по решению автора.
