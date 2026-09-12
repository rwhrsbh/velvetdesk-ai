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

## Админка

`http://<адрес>/admin` — одна страница: провайдеры и их ключи, модели с ценами
и порядком падения, тарифы, выдача и отзыв лицензий, расход по лицензиям и
моделям. Вход по `VD_ADMIN_TOKEN` (заголовок `X-VD-Admin`), токен хранится
только во вкладке браузера.

Всё, что правится в админке, лежит в базе, а не в `gateway.json`: конфиг
засевает пустую базу при первом запуске и дальше не читается. Добавленный ключ
попадает в пул к следующему запросу — перезапуск не нужен. Ключи наружу не
отдаются: список показывает маску вида `AIzaS...9fA` и id для удаления.

Лицензия подписывается тем же приватным ключом, что и `mint`, и показывается
один раз — шлюз хранит только факт выдачи.

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
| GET | `/admin` | страница управления (`X-VD-Admin` на запросах из неё) |
| GET/POST | `/admin/upstreams`, `/admin/models`, `/admin/tiers` | список и сохранение |
| DELETE | `/admin/upstreams/<id>`, `/admin/models/<name>`, `/admin/tiers/<name>` | удаление |
| GET/POST | `/admin/upstreams/<id>/keys` | ключи провайдера: маски и добавление |
| DELETE | `/admin/keys/<id>` | удалить ключ |
| GET/POST | `/admin/licenses` | выданные лицензии и выдача новой |
| POST | `/admin/revoke`, `/admin/unrevoke` | отозвать и вернуть |
| GET | `/admin/stats?hours=24` | расход по лицензиям и моделям, доля кеша |

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


## Docker

```sh
export VD_ADMIN_TOKEN='что-нибудь длинное'
export VD_LICENSE_PUBLIC_KEY='вывод keygen'
docker compose up -d --build
```

Всё состояние — в `./gateway-data`: база, конфиг, ключи. Переезд на другой
сервер: остановить, скопировать папку, поднять там. Порт открыт только на
loopback — TLS ставится впереди (caddy, nginx).

Первый запуск с пустым томом работает: шлюз пишет стартовый конфиг и
поднимается, а провайдеры, ключи, модели и тарифы добавляются в `/admin`.

Ключ для подписи лицензий делается один раз и лежит вне контейнера:

```sh
docker compose run --rm gateway keygen     # приватный ключ — в ./gateway-data
docker compose run --rm gateway mint acme-1 business 365 10
```

## Очередь

`max_inflight`, `max_per_license`, `max_queued`, `queue_wait_seconds` в
конфиге. Текущее состояние — на вкладке «Провайдеры и ключи» в админке.

## Голос

Модель с флагом `voice` обслуживает `POST /v1/audio/transcriptions`
(multipart, как у OpenAI) и не предлагается в чате. Считается по
`price_request` — цене за клип в долларах.
