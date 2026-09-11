# VelvetDesk gateway

Один бинарь на VPS: принимает запросы клиента по лицензии, отправляет их
наверх своими ключами, считает кредиты и ведёт учёт.

## Запуск

```sh
cargo build --release -p velvetdesk-gateway

# ключ, которым подписываются лицензии: приватная половина в файл, публичная в конфиг
VD_LICENSE_KEY_FILE=/etc/velvetdesk/license-signing.key ./velvetdesk-gateway keygen

cp server-gateway/gateway.example.json /etc/velvetdesk/gateway.json
# вписать license_public_key из вывода keygen и ключи провайдеров

export OPENROUTER_KEY=sk-or-...
export GEMINI_KEYS=key1,key2,key3      # одна переменная может держать пул
export VD_ADMIN_TOKEN=...              # без него /admin/* закрыт
VD_GATEWAY_CONFIG=/etc/velvetdesk/gateway.json ./velvetdesk-gateway serve
```

Лицензия на 30 дней:

```sh
./velvetdesk-gateway mint VD-PRO-0001 pro 30 2
```

`0` вместо числа дней — лицензия без срока.

## Эндпоинты

| Метод | Путь | Что делает |
| --- | --- | --- |
| GET | `/health` | жив ли |
| GET | `/v1/models` | список моделей |
| GET | `/v1/usage` | остаток кредитов по лицензии |
| POST | `/v1/chat/completions` | OpenAI-совместимый, `stream` поддержан |
| POST | `/v1beta/models/<model>:generateContent` | нативный Gemini |
| POST | `/v1beta/models/<model>:streamGenerateContent` | он же потоком |
| GET | `/sync/ws?room=<id>` | релей синхронизации: перекладывает запечатанные кадры между устройствами одной пары |
| POST | `/admin/revoke` | отозвать лицензию (`X-VD-Admin`) |
| GET | `/admin/stats?hours=24` | доля попаданий кеша (`X-VD-Admin`) |

Лицензия едет в `Authorization: Bearer VD.…`, в `x-goog-api-key` или в `?key=`.
В ответе — `X-VD-Credits-Left` и `X-VD-Window-Reset`.

## Учёт

Кредит — единица себестоимости: `credit_usd` в конфиге говорит, сколько он
стоит в долларах, цены моделей заданы за миллион токенов. Кешированные
промпт-токены считаются по своей цене, остальные по полной. Лимит — два окна
сразу: скользящие 5 часов и неделя; при исчерпании `429` с `reset_at`.

## Синхронизация

Шлюз в синхронизации участвует только как почтальон: устройства пары приходят
в комнату `/sync/ws?room=<хеш ключа пары>` и обмениваются кадрами
XChaCha20-Poly1305. Ключ пары шлюз не видит и расшифровать ничего не может;
сколько устройств пускать в комнату, говорит `max_peers` лицензии.

## systemd

```ini
[Unit]
Description=VelvetDesk gateway
After=network.target

[Service]
User=velvetdesk
Environment=VD_GATEWAY_CONFIG=/etc/velvetdesk/gateway.json
Environment=VD_LICENSE_KEY_FILE=/etc/velvetdesk/license-signing.key
EnvironmentFile=/etc/velvetdesk/keys.env
ExecStart=/usr/local/bin/velvetdesk-gateway serve
Restart=always

[Install]
WantedBy=multi-user.target
```

TLS терминирует nginx или caddy перед ним; сам шлюз слушает по HTTP на
localhost.
