# Куда и как распространять решение (Distribution & Upstream Plan)

Ниже разобраны реальные площадки и репозитории, где владельцы Gigabyte U4 UD (`RC14UD`) и Clevo `L14xMU` ищут решение проблемы с ограничением заряда, а также точный план публикации.

---

## 1. Стоит ли делать отдельный репозиторий?

**Да — однозначно стоит.** Причины:
1. Апстрим `tuxedo-drivers` (GitLab) принимает **только минимальный патч ядра** — без Rust-демона `u4-charge`, без systemd-юнитов, без KDE-уведомлений и без Arch `PKGBUILD`.
2. Самый популярный драйвер для ноутбуков Gigabyte на Linux — `tangalbert919/gigabyte-laptop-wmi` — **официально не поддерживает всю U-серию** (включая Gigabyte U4), так как U4 собран на платформе Clevo (`L14xMU` / EC IT5570E), а не на WMI `WMBC`/`WMBD`. Пользователи упираются в тупик.
3. Отдельный репозиторий (например, **`gigabyte-u4-charge`**) даёт готовое решение «из коробки» (DKMS-патч + безопасный Rust-демон + KDE-интеграция + инструкция) и служит точкой ссылки для issues/MR в апстрим.

---

## 2. Каналы распространения (от приоритетного к дополнительному)

### Канал 1: Собственный GitHub-репозиторий (`gigabyte-u4-charge`)
- **Что публиковать:** содержимое подготовленного каталога `gigabyte-u4-charge/` (`README.md`, `README.ru.md`, `docs/HARDWARE_NOTES.md`, `kernel/`, `daemon/`, `systemd/`, `install.sh`, `LICENSE`).
- **Важно:** предварительно проверь, что `kernel/PKGBUILD` смотрит на официальный апстрим `git+https://gitlab.com/tuxedocomputers/development/packages/tuxedo-drivers.git#tag=v${pkgver}`, а не на локальный путь.

### Канал 2: Апстрим `tuxedo-drivers` на GitLab (главная цель для ядра!)
- **Куда:** [https://gitlab.com/tuxedocomputers/development/packages/tuxedo-drivers](https://gitlab.com/tuxedocomputers/development/packages/tuxedo-drivers)
  *(GitHub-репозиторий `tuxedocomputers/tuxedo-drivers` — только read-only зеркало; Merge Requests и Issues нужно подавать именно на GitLab).*
- **Что отправлять:** **только минимальный патч** [`kernel/upstream-tuxedo-drivers-l14xmu.patch`](../kernel/upstream-tuxedo-drivers-l14xmu.patch) без `ec_live_state` и с DMI-ограничением `_DSM(0x04)` строго на `clevo_uses_atomic_baee_fc()`, чтобы не затронуть другие ноутбуки Clevo/TUXEDO, плюс выжимку из `docs/HARDWARE_NOTES.md` (эмпирические замеры `0x41` vs `0x76` и latch-эффект).

### Канал 3: Форк для Arch/Clevo — `nick42d/clevo-drivers` (GitHub) и AUR
- **Куда:** [https://github.com/nick42d/clevo-drivers](https://github.com/nick42d/clevo-drivers) (апстрим для AUR-пакета `clevo-drivers-dkms-git`).
- **Что делать:** открыть Issue / Pull Request со ссылкой на минимальный патч и описанием проблемы `L14xMU` / Gigabyte U4 UD. Если мейнтейнер примет патч или предложит отдельный пакет, пользователи Arch смогут обновляться прямо из AUR.
- **Опционально — AUR:** после тестирования на вашей системе можно опубликовать в AUR пакет `clevo-drivers-u4-dkms` (или `u4-charge`), сославшись на ваш GitHub-репозиторий.

### Канал 4: `linrunner/TLP` (GitHub, Issue #803)
- **Куда:** [https://github.com/linrunner/TLP](https://github.com/linrunner/TLP) (тема поддержки порогов Clevo/TUXEDO, Issue #803).
- **Зачем:** TLP 1.9+ работает поверх `clevo_acpi` (`charge_control_*_threshold`), но пользователи Clevo `L14xMU` и Gigabyte U4 сталкиваются с тем, что непатченный `clevo_acpi` либо скрывает интерфейс, либо использует `0x76` / защёлкивается при повышении порога. Короткий комментарий со ссылкой на MR/репозиторий сэкономит время разработчикам TLP и пользователям `tlp-stat -b`.

### Канал 5: Профильные проекты и сообщества (чтобы люди реально находили)
1. **`tangalbert919/gigabyte-laptop-wmi`** (GitHub Issues/Discussions) — вежливо предложить добавить в README рядом с пометкой *"All U series models (not supported)"* ссылку на решение для Clevo-ODM U4.
2. **GNOME `maniacx/Battery-Health-Charging` / KDE сообщества:** упомянуть совместимость со стандартным `BAT0/charge_control_*` и KDE PowerDevil 6 после патча `clevo_acpi`.
3. **ArchWiki:** разделы по ноутбукам Gigabyte / Clevo / `Laptop/Power management` (краткая заметка + ссылка только после стабилизации и принятия MR/AUR по правилам ArchWiki).
4. **Статьи / посты:**
   - **Reddit:** `r/linuxhardware`, `r/archlinux`, `r/gigabytegaming` — пост в формате *"Fixing hardware battery charge thresholds on Gigabyte U4 / Clevo L14xMU (ITE IT5570E EC)"*.
   - **Habr (RU):** отличная техническая статья про реверс-инжиниринг DSDT + EC ITE IT5570E, бит `FBF1=0x02` в `ZEVT(0x41)` и безопасный демон на Rust без `unsafe`.
