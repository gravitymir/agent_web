# Go-live runbook — админ + гостевой доступ (magic-links)

Эксплуатационная памятка: как поднять систему «наружу» и выдать гостю доступ.
Архитектура executor'а и провижнинг снапшота — в [`EXECUTOR.md`](EXECUTOR.md).

## Топология

```
                         Cloudflare Tunnel "agent-web" (token-managed)
                         ├─ agent.astechlab.dev  ──▶ localhost:8787   (ХОСТ, admin)
                         │     за Cloudflare Access (только владелец)
                         └─ guest.astechlab.dev  ──▶ localhost:8788   (NAT ▶ executor:8787)
                               без Access; вход по magic-link (CWI_AUTH)
```

- **Admin (хост, `:8787`)** — мастер-страница. `CWI_AUTH` выключен ⇒ `admin=true`:
  видна вкладка «Ссылки» (выпуск гостевых ссылок) и «Гостевой сервер» (control VM).
  Защита — внешняя, Cloudflare Access на `agent.astechlab.dev`.
- **Guest (executor VM, `:8787` ← NAT `:8788`)** — тот же бинарник с `CWI_AUTH=1`,
  `CWI_ADMIN=0`: гость входит только по magic-link, админ-роуты отвечают 403.
  Агент гостя крутится **в VM**; модельные ответы идут через брокер на хосте
  (`/broker/v1/messages`), реальный ключ VM не видит.

## Разовая настройка (уже сделано)

| Что | Где | Статус |
|-----|-----|--------|
| CF hostname `agent.astechlab.dev → localhost:8787` | CF dashboard, tunnel `agent-web` | ✅ + Access |
| CF hostname `guest.astechlab.dev → localhost:8788` | CF dashboard, tunnel `agent-web` | ✅ без Access |
| `CWI_GUEST_URL=https://guest.astechlab.dev` | хостовый `.env` | ✅ |
| Снапшот `clean` с `CWI_AUTH=1`, `CWI_ADMIN=0` | VM `executor` | ✅ (UUID `6a22d823`) |
| NAT `aw` 8788→8787, `ssh` 2222→22 | VM `executor` | ✅ (в снапшоте) |
| systemd `agent-web.service` (автозапуск, broker-режим) | внутри гостя | ✅ (в снапшоте) |

## Последовательность запуска (go-live)

1. **cloudflared** (нужны права администратора — служба Windows):

   ```powershell
   # из-под обычного PowerShell — откроет UAC и стартанёт службу:
   Start-Process powershell -Verb RunAs -ArgumentList '-Command','Start-Service cloudflared'
   # проверка (без прав):
   (Get-Service cloudflared).Status   # ожидаем Running
   ```

2. **Host-app** — запусти (или **перезапусти**, если уже был запущен до правки
   `.env`: `CWI_GUEST_URL` читается только на старте). Мастер-страница —
   `https://agent.astechlab.dev` (пустит после Cloudflare Access).

3. **Executor** — на мастер-странице вкладка «Гостевой сервер» → **Start**.
   Restore снапшота → буст → systemd поднимает `agent_web` → хост пушит текущие
   гостевые коды в VM. Готово за ~30 c (статус «Работает»).

4. **Выпуск ссылки** — вкладка «Ссылки» → метка + срок → «Создать ссылку».
   Ссылка вида `https://guest.astechlab.dev/login?code=…`, показывается один раз.
   Коды пушатся в запущенный executor сразу (на mint) и на каждый Start.

## Проверки

```bash
# executor поднят локально:
curl -s http://localhost:8788/api/health                       # {"status":"ok",…}
# гость доходит через туннель (cloudflared + executor вверх):
curl -s -o /dev/null -w "%{http_code}\n" https://guest.astechlab.dev/api/health   # 200
# без кода — редирект на вход:
curl -s -o /dev/null -w "%{http_code} %{redirect_url}\n" https://guest.astechlab.dev/   # 303 …/login
```

Финальная проверка: открой выпущенную ссылку в приватном окне — должно пустить
в чат.

## Troubleshooting

| Симптом | Причина | Что делать |
|---------|---------|-----------|
| `https://guest…` → **530 / error 1033** | cloudflared не запущен | `Start-Service cloudflared` (admin) |
| `https://guest…/api/health` → **502/504** | executor выключен | «Гостевой сервер» → Start |
| Ссылка ведёт на `localhost:8788`, а не на домен | host-app не перезапущен после `.env` | перезапустить host-app |
| Гость входит без кода | на `guest.astechlab.dev` повесили Access-политику или `CWI_AUTH` не включён в VM | снять Access; проверить `CWI_AUTH=1` в `~/agent-web.env` |
| Вкладки «Ссылки»/«Гостевой сервер» не видны | `admin=false` (сработал `CWI_AUTH`/`CWI_ADMIN`) | на хосте гейт должен быть off, либо `CWI_ADMIN=1` |
| Ссылка не пускает гостя (валидный код) | коды не доехали в VM | Start заново (пушит коды), или проверь SSH-доступ к VM |

## Где что лежит (не секретить в git)

- Гостевые коды (хэши): хост `…/chats/guest_tokens.json`; в VM
  `/home/insider/agent_web/target/release/chats/guest_tokens.json` (пушится с хоста).
- Broker-токены: хост `…/chats/broker_tokens.json`; в VM — в `~/agent-web.env`.
- SSH в VM: `insider@127.0.0.1:2222`, ключ `~/.ssh/agent_vm_key`.
- Реальные провайдер-ключи — только в хостовом `.env` (git-ignored). Executor их
  не видит — ходит через брокер.
