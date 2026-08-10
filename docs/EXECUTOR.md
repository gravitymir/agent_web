# Executor VM + брокер

Изолированное выполнение агента в одноразовой виртуальной машине, при котором
**реальный API-ключ никогда не покидает хост**.

Хост держит панель управления и «брокер» (прокси с настоящим ключом). Одноразовая
VirtualBox-машина (`executor`) запускает **тот же** `agent_web` в режиме `broker`:
она ходит за инференсом не напрямую к провайдеру, а на брокер хоста, предъявляя
короткоживущий **session-token** вместо ключа. Скомпрометированный гость выдаёт
только токен (ограниченный по времени и бюджету, отзываемый), но не ключ.

```
┌────────────────────────── ХОСТ (Windows) ──────────────────────────┐
│                                                                     │
│  agent_web  (интерактивный выбор движка: Cloud / Anthropic / …)     │
│   ├─ Панель управления + вкладка «Гостевой сервер» (WebSocket)      │
│   ├─ Брокер   POST /broker/v1/messages                             │
│   │     авторизует Bearer-токен → подставляет РЕАЛЬНЫЙ ключ →       │
│   │     переписывает model на хостовую → форвардит провайдеру       │
│   └─ Управление VM: VBoxManage + SSH (src/executor.rs)             │
│                                                                     │
│         VirtualBox NAT:  ssh 2222→22   aw 8788→8787                 │
│                              ▲                                       │
└──────────────────────────────┼─────────────────────────────────────┘
                                │ 10.0.2.2 = шлюз → loopback хоста
┌──────────────────── VM «executor» (Ubuntu 26.04) ───────────────────┐
│  systemd: agent-web.service  (CWI_ENGINE=native, provider=broker)   │
│   bind 0.0.0.0:8787                                                 │
│   base_url = http://10.0.2.2:8787/broker                           │
│   api_key  = <broker session-token>                                │
└─────────────────────────────────────────────────────────────────────┘
```

---

## 1. Брокер (`src/broker.rs`)

Прокси на хосте, который держит настоящий ключ и раздаёт гостю одноразовые токены.

### Токены — CLI

```bash
agent_web broker new  [--label executor] [--ttl 24h] [--budget 0]
agent_web broker list
agent_web broker revoke <label>
```

- Токен показывается **один раз**; на диск пишется только его SHA-256
  (`~/.claude/broker_tokens.json`). Тот же файл читает работающий сервер, поэтому
  токен, выпущенный через CLI, валиден для запущенного брокера.
- `--ttl`: `30m` / `24h` / `7d` (голое число = секунды).
- `--budget`: лимит запросов (`0` = без лимита). Каждый проксированный запрос
  списывает единицу; исчерпание → `402 Payment Required`.
- Просроченные токены отсеиваются при каждом `new`/`list`.

### Форвардинг — `POST /broker/v1/messages`

1. Извлечь `Authorization: Bearer <token>`, авторизовать, списать бюджет.
2. Взять провайдера хоста через `Provider::from_env()` (то, что выбрано в
   интерактивном wizard'е — см. §5).
3. **Переписать `model`** в теле на хостовую (`provider.model`): гость не знает и
   не выбирает модель — какой ценой платить, решает владелец ключа.
4. Форварднуть на `provider.messages_url()` с реальной авторизацией
   (`x-api-key` / `Bearer` / `x-goog-api-key`), стримом вернуть ответ.

> **Важно для боевого прогона.** Брокер форвардит тело в формате Anthropic
> `/v1/messages`, меняя только `model`. Значит хостовый провайдер должен понимать
> этот формат — выбирай в wizard'е **«Native — Anthropic API»** (реальный
> console-ключ). Gemini/прочие с другим API-контрактом для брокера не подойдут.
> Гостевой конфиг при этом менять не нужно.

---

## 2. Одноразовая VM (`src/executor.rs`)

Тонкие обёртки над `VBoxManage` и SSH. Операции блокирующие (шелл-ауты), поэтому
асинхронный сервер вызывает их через `spawn_blocking`.

Соглашения (зашиты в снапшот `clean` и в NAT-форварды):

| Параметр | Значение |
|----------|----------|
| Имя VM | `executor` |
| Снапшот | `clean` |
| SSH | `127.0.0.1:2222` → гость `:22`, пользователь `insider`, ключ `~/.ssh/agent_vm_key` |
| Порт приложения | NAT `aw`: хост `127.0.0.1:8788` → гость `:8787` |
| Порт гостя внутри | `0.0.0.0:8787` |

Ключевые функции: `restore_clean()`, `start_headless()`, `stop_graceful()`
(ACPI-кнопка → через 10 с жёсткий `poweroff`), `ssh_ready()`, `ssh_run(cmd)`,
`status() -> {exists, running, ssh_ready, has_clean_snapshot}`.

**Disposable-цикл:** `restore clean` → холодный буст → systemd сам поднимает
`agent-web` → приложение доступно с хоста через `:8788` (~12 с). Каждый запуск —
чистая машина из снапшота, без следов прошлой сессии.

---

## 3. Вкладка «Гостевой сервер» (управление по WebSocket)

Слева на главной странице (`static/js/guest.js`, панель `#guest-view`). Кадры
`{type:"executor", action}` идут по тому же `/ws`; сервер отвечает
`{cwi:"executor", state, vm, active_turns}`, фронт обновляет индикатор/лог.

| Кнопка | Действие | Что делает |
|--------|----------|-----------|
| **Start** | `start` | `restore_clean` + `start_headless`, ждёт SSH-готовности |
| **Stop** | `stop` | `stop_graceful` (ACPI → жёсткий poweroff) |
| **Drain-Stop** | `drain` | мягкая остановка: **дождаться гостевых агентов**, затем стоп |

### Семантика Drain-Stop (`handle_drain` в `src/ws.rs`)

Пользователь внутри гостя может вести активную сессию (агент «думает»). Нельзя
рубить машину под ним. Поэтому:

1. `POST http://127.0.0.1:8788/api/drain/begin` — перевести гостя в режим слива
   (новые ходы не начинаются).
2. Поллить `GET /api/health` гостя → `active_turns`, пока не станет `0`
   (до ~10 минут, шаг 5 с).
3. `ssh_run("sudo systemctl stop agent-web")` — остановить приложение в гостe.
4. `stop_graceful()` — выключить VM.

> Имя сервиса `agent-web` **обязано** совпадать с командой в шаге 3 — оно так и
> задаётся при провижнинге (§4).

На стороне хоста слив опирается на уже существующий `SessionManager`
(`set_draining` / `active_turns`) и эндпоинты `POST /api/drain/begin`,
`GET /api/health` (оба — в `src/main.rs`), которые обслуживает гостевой `agent_web`.

---

## 4. Что запечено в снапшот `clean` (провижнинг)

Одноразовая база собирается один раз и фиксируется снапшотом. Внутри гостя:

| Компонент | Назначение |
|-----------|-----------|
| Беспарольный sudo для `insider` (`/etc/sudoers.d/90-insider`) | нужен для runtime `systemctl stop agent-web` при Drain-Stop |
| `build-essential`, `pkg-config`, Rust (rustup, minimal) | сборка `agent_web` в гостe |
| `~/agent_web` (клон + `cargo build --release`) | сам бинарник executor'а |
| `~/agent-web.env` (chmod 600) | конфиг брокер-режима (см. ниже) |
| `agent-web.service` (systemd, enabled) | автозапуск приложения на буст |

Гостевой `~/agent-web.env`:

```ini
CWI_ENGINE=native
CWI_AGENT_PROVIDER=broker
CWI_AGENT_API_KEY=<broker session-token>
CWI_AGENT_BASE_URL=http://10.0.2.2:8787/broker
CWI_BIND=0.0.0.0:8787
CWI_AUTH=1     # включает гейт: гость входит только по magic-link
CWI_ADMIN=0   # это гость — админ-роуты (/api/links, VM) отвечают 403
```

### Гостевые magic-link'и (выпуск с мастер-страницы)

Ссылки выпускаются **только с мастер-страницы** (вкладка «Ссылки»,
`POST /api/links`) — она за внешним гейтом туннеля (Cloudflare Access).
Проверяются коды на executor'е (у него `CWI_AUTH=1`).

- Хранилище кодов на хосте: `<config>/guest_tokens.json` (только SHA-256 хэши).
- Executor одноразовый — его хранилище чистое после каждого restore. Хост
  **пушит** активные коды в гостя по SSH (`executor::push_guest_tokens`,
  `cat > …/target/release/chats/guest_tokens.json`) при каждом Start, а также
  на каждый mint/revoke, пока VM запущена. `verify_code` читает файл вживую —
  рестарт не нужен.
- База в magic-link'е = `CWI_GUEST_URL` на хосте (иначе `CWI_PUBLIC_URL`,
  иначе `http://localhost:8788`). В проде укажи гостевой домен туннеля,
  например `https://guest.astechlab.dev` → cloudflared → host:8788 → NAT →
  executor:8787.

Юнит `/etc/systemd/system/agent-web.service`:

```ini
[Unit]
Description=agent_web (executor, broker mode)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=insider
WorkingDirectory=/home/insider/agent_web
EnvironmentFile=/home/insider/agent-web.env
ExecStart=/home/insider/agent_web/target/release/agent_web
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
```

### Пере-провижнинг (обновить бинарник/конфиг и пересобрать снапшот)

```bash
# 0) поднять текущий clean для правок
VBoxManage snapshot executor restore clean
VBoxManage startvm executor --type headless

SSH="ssh -i ~/.ssh/agent_vm_key -p 2222 insider@127.0.0.1"

# 1) обновить код и пересобрать
$SSH 'cd ~/agent_web && git pull && ~/.cargo/bin/cargo build --release'

# 2) (при смене токена) перевыпустить на хосте и переписать env
agent_web broker new --label executor --ttl 365d --budget 0   # хост
$SSH 'sed -i "s#^CWI_AGENT_API_KEY=.*#CWI_AGENT_API_KEY=<TOKEN>#" ~/agent-web.env'
$SSH 'sudo systemctl restart agent-web'

# 3) выключить и пересобрать снапшот
VBoxManage controlvm executor acpipowerbutton   # подождать выключения
VBoxManage snapshot executor delete clean
VBoxManage snapshot executor take   clean --description "…"
```

---

## 5. Эксплуатация

1. **Хост.** Запусти `agent_web`; в интерактивном меню выбери движок. Для реального
   агента в гостe нужен **«Native — Anthropic API»** (брокер форвардит в
   Anthropic-формате). Порт хоста — `8787`.
2. **Гость.** Кнопкой **Start** на вкладке «Гостевой сервер» подними одноразовую VM.
   Через ~12 с гостевой `agent_web` доступен (внешний вход настраивается отдельно —
   напр. Cloudflare Tunnel `guest.<домен>` → `localhost:8788`, с Cloudflare Access).
3. **Остановка.** **Drain-Stop** — если внутри есть активные сессии (дождётся их).
   **Stop** — немедленно.

---

## Связанные файлы

- `src/broker.rs` — брокер (форвардинг + CLI токенов).
- `src/executor.rs` — управление VM (VBoxManage + SSH).
- `src/ws.rs` — WS-обработчики `executor` / `drain`.
- `static/js/guest.js`, `static/index.html`, `static/styles.css` — вкладка «Гостевой сервер».
- `docs/HARDENING.md` — модель угроз и укрепление периметра.
