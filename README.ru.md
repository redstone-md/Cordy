<div align="center">

<img src="https://gfx.redstone.md/strip?label=Cordy&note=терминальный%20кодинг-агент%20на%20Rust&theme=dark" alt="Cordy" width="840">

[English](README.md) · **Русский**

Меняй модель и провайдера прямо посреди диалога. Правь файлы за разрешительным гейтом.
MCP, скиллы, суб-агенты и фоновые процессы — в одном быстром TUI, управляемом с клавиатуры.

[![CI](https://github.com/redstone-md/Cordy/actions/workflows/ci.yml/badge.svg)](https://github.com/redstone-md/Cordy/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/redstone-md/Cordy?display_name=tag&sort=semver)](https://github.com/redstone-md/Cordy/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-6e9bf5)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024-b7410e)](https://www.rust-lang.org)

</div>

Cordy — кросс-платформенный терминальный агент, который работает со всеми основными семействами
API — OpenAI Chat Completions, OpenAI-совместимые эндпоинты, Anthropic Messages и OpenAI Responses
API — через единую каноническую модель, поэтому смена провайдера или модели не теряет диалог. Он
правит файлы точным `edit` с подтверждением диффа, запускает встроенный набор инструментов, вывод
которых сжимается нативным оптимизатором токенов, и говорит на Model Context Protocol.

<img src="https://gfx.redstone.md/strip?label=Highlights&theme=dark" alt="Возможности" width="840">

- **Горячая смена модели и провайдера** посреди диалога — начни на локальной модели, закончи на
  топовой, контекст цел. `^P` → выбор модели, или `/connect` для нового провайдера.
- **Все семейства API** — OpenAI Chat, OpenAI-совместимые (Ollama / vLLM / OpenRouter / Groq / …),
  Anthropic Messages, OpenAI Responses. Форматы протокола не выходят за пределы адаптера.
- **Встроенные инструменты** — `read` `write` `edit` `multiedit` `grep` `glob` `ls` `bash` `todo`
  `web_search` `web_fetch` `process` `rewind`, все за разрешительным гейтом с pre-approve по glob.
- **Фоновые процессы** — запусти дев-сервер через `bash background:true`, затем `process` — опрос,
  ожидание по регулярке, убийство всего дерева процессов.
- **MCP, скиллы, суб-агенты** — подключай MCP-серверы, загружай скиллы с прогрессивным раскрытием,
  делегируй работу изолированным суб-агентам через инструмент `task`.
- **Бесплатный веб-поиск** — DuckDuckGo из коробки (без ключа), или Exa при заданном `EXA_API_KEY`.
- **Живой HUD стоимости и контекста** — токены, `$`-стоимость, экономия оптимизатора, заполнение
  контекста, фоновые задачи и суб-агенты в боковой панели.
- **Интерфейс в духе OpenCode** — палитра команд, история промптов, многострочный ввод, сессии,
  менеджер провайдеров, восемь тем и полный хот-релоад конфига.

<img src="https://gfx.redstone.md/strip?label=Install&theme=dark" alt="Установка" width="840">

**Готовые бинарники** — скачай архив под свою платформу из
[последнего релиза](https://github.com/redstone-md/Cordy/releases/latest), распакуй и запусти `cordy`.

**Из исходников** (Rust 1.85+):

```sh
git clone https://github.com/redstone-md/Cordy.git
cd Cordy
cargo build --release
# бинарник в target/release/cordy
```

Опциональный MCP-клиент (тянет более тяжёлый SDK `rmcp`):

```sh
cargo build --release --features mcp
```

<img src="https://gfx.redstone.md/strip?label=Quick%20start&theme=dark" alt="Быстрый старт" width="840">

Укажи Cordy любой OpenAI-совместимый эндпоинт через переменные окружения и запусти:

```sh
export OPENAI_API_KEY=sk-...
export CORDY_BASE_URL=https://api.openai.com/v1   # или любой совместимый эндпоинт
export CORDY_MODEL=gpt-4o-mini
cordy
```

…или пропусти переменные и нажми **`^P` → /connect** внутри приложения, чтобы добавить провайдера
через визард. Подключённые провайдеры сохраняются в `~/.cordy/config.toml`, а последний
восстанавливается при запуске. Всё пользовательское состояние — конфиг, ключи, сессии, кеш —
живёт под `~/.cordy`.

<img src="https://gfx.redstone.md/strip?label=Keybinds&theme=dark" alt="Клавиши" width="840">

Клавиши повторяют OpenCode. Основное:

| Клавиша | Действие |
| --- | --- |
| `Enter` | отправить · `^J` / `Alt+Enter` новая строка |
| `Tab` / `Shift+Tab` | цикл агента/режима |
| `^P` | палитра команд — модели, агенты, темы, провайдеры, команды |
| `↑` / `↓` | история промптов · движение по строкам |
| `Esc` | прервать текущий ход |
| `^X` затем … | лидер: `l` сессии · `m` модели · `t` тема · `s` статус · `y` копировать ответ |
| `^R` · `^L` · `^-`/`^.` | переименовать сессию · перерисовать · undo/redo ввода |
| колесо · `PgUp`/`PgDn` · `^G` | скролл ленты |

Слэш-команды: `/connect` `/providers` `/model` `/sessions` `/permissions` `/mouse` `/thinking`
`/compact` `/goal` `/ralph` и другие — набери `/` для автодополнения.

<img src="https://gfx.redstone.md/strip?label=Configuration&theme=dark" alt="Конфигурация" width="840">

`~/.cordy/config.toml` (сливается с проектным `.cordy/config.toml`). **Хот-релоад** — правки
применяются в течение секунды, без перезапуска. Секретов здесь нет; API-ключи — в переменных
окружения или `~/.cordy/keys.json` (пишется через `/connect`).

```toml
optimize = true
theme = "tokyonight"   # dark · tokyonight · catppuccin · gruvbox · nord · rosepine · light · mono

# Точечная настройка любого цвета поверх темы
[colors]
accent  = "#7aa2f7"
surface = "#24283b"

# Заранее разрешить команды, чтобы агент не спрашивал
[permissions]
mode  = "ask"          # или "auto"
allow = ["bash:ls *", "bash:git *", "bash:cargo *"]
deny  = ["bash:rm -rf *"]

# Провайдер (секреты — в env / keys.json, не здесь)
[[provider]]
name = "openai"
kind = "openai-chat"
base_url = "https://api.openai.com/v1"

# MCP-сервер (нужна сборка с --features mcp)
[[mcp]]
name = "browsermcp"
transport = "stdio"
command = "npx @browsermcp/mcp@latest"
enabled = true
```

<img src="https://gfx.redstone.md/strip?label=Project%20layout&theme=dark" alt="Структура" width="840">

Один бинарный крейт, границы модулей — через трейты и `mod`-приватность:

```
src/
  core/       каноническая модель, agent loop, промпт, контекст, сессии, права,
              автономный (ralph) цикл, auth
  provider/   трейт Provider + по адаптеру на семейство API (+ retry-декоратор)
  tools/      трейт Tool, встроенные, нативный оптимизатор вывода, суб-агенты, фоновые джобы
  skills/     загрузчик скиллов с прогрессивным раскрытием
  agents/     реестр суб-агентов
  mcp/        MCP-клиент (за фичей)
  config/     загрузка/слияние TOML, профили моделей и провайдеров
  tui/        ratatui MVU-приложение — model, update, view, темы, markdown
```

<img src="https://gfx.redstone.md/strip?label=Contributing&theme=dark" alt="Вклад" width="840">

Вклады приветствуются — см. [CONTRIBUTING.md](CONTRIBUTING.md). Перед PR:

```sh
cargo fmt
cargo clippy --all-targets
cargo test
```

<img src="https://gfx.redstone.md/strip?label=License&theme=dark" alt="Лицензия" width="840">

Распространяется под [лицензией MIT](LICENSE).

<div align="center">
<sub>Часть <a href="https://github.com/redstone-md">redstone.md</a></sub>
</div>
