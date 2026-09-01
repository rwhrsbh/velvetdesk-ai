# VelvetDesk AI

Локальный мультиагентный копилот для операторов дейтинг-агентства. Tauri 2 + Rust,
одна кодовая база на **Windows, macOS, Linux и Android**. Данные не покидают устройство.

![build](https://img.shields.io/badge/build-github%20actions-blue) ![license](https://img.shields.io/badge/license-MIT-green)

---

## Что внутри

| Слой | Реализация |
| --- | --- |
| UI | Vite + TypeScript, Apple-HIG glassmorphism, трёхколоночный воркспейс, мобильный режим с таб-баром |
| Ядро | Rust: изолированное хранилище, движок агентов, доктор, пул ключей |
| Хранилище | JSON-файлы в `AppData/app_data/`, атомарная запись через `.tmp` + rename |
| LLM | Google Gemini (v1/v1beta, native function calling) и любой OpenAI-совместимый endpoint |

### Мультиагентная иерархия

```
Master Agent (глобальный)         — кросс-поиск, авто-маршрутизация сырого текста, авто-создание досье
├── Model Agent (в песочнице)     — чат/письма, тон, мутации CRM
├── Doctor Agent                  — валидация схем, починка JSON, целостность индекса
└── Background Memory             — однопроходная синхронизация фактов, подарков, стадий
```

### Режимы работы

| Режим | Что делает | Стоимость |
| --- | --- | --- |
| **AUTO** | Модель сама вызывает инструменты: читает досье и историю, пишет ответ, сохраняет факты | несколько вызовов |
| **ACT** | Один вызов → `{reply, memory_patch}`: черновик + патч памяти | 1 вызов |
| **MEMORIZE** | Один вызов → только патч памяти, ответ не генерируется (режим диктовки) | 1 вызов |

### Уровни безопасности

| Уровень | Чтение | Добавление/обновление | Удаление, переписывание персоны |
| --- | --- | --- | --- |
| **Ask** | сразу | подтверждение | подтверждение |
| **Safe** (по умолчанию) | сразу | сразу | подтверждение |
| **Full** | сразу | сразу | сразу |

Каждое отложенное действие показывается как diff «до / после» в очереди (🛡 в топбаре).
Подтверждение перепланирует мутацию заново на актуальных данных — устаревшее в диск не попадёт.

---

## Изоляция профилей

```
app_data/
  settings.json
  secrets.json                       # API-ключи, chmod 600 на unix
  global_index.json
  profiles/<model_id>/profile.json
  profiles/<model_id>/men/<man_id>.json
  profiles/<model_id>/chats/<man_id>.json
  profiles/<model_id>/agent_log.json
  profiles/<model_id>/attachments/
```

Агент модели получает `Scope`, который:

- принимает только id из `[A-Za-z0-9_-]{1,64}` — точки и слэши отсекаются;
- отклоняет абсолютные пути и любые `..` компоненты;
- канонизирует родительскую директорию и проверяет, что она лежит внутри песочницы (защита от симлинков).

Соседние профили физически недоступны — проверено тестами (`storage::tests::rejects_traversal`).

## Ротация ключей

Пул на провайдера, round-robin. Вердикты и кулдауны:

| Код | Вердикт | Кулдаун (× число последовательных сбоев, max ×4) |
| --- | --- | --- |
| 429 | RateLimited | 60 с |
| 401 / 403 | QuotaOrAuth | 900 с |
| 5xx | ServerError | 15 с |
| таймаут / обрыв | Transient | 5 с |
| прочие 4xx | Fatal | без кулдауна, ротация только если ключей > 1 |

Между попытками экспоненциальный backoff 1 → 2 → 4 → 8 с, максимум `min(ключи × 2, 8)` попыток.
UI показывает ротацию в реальном времени (событие `velvetdesk://agent`).

## Доктор

`🩺` в топбаре: сухой прогон (scan) и авто-исправление (fix).

- чинит битый JSON: BOM, висячие запятые, незакрытые строки и скобки, мусор после объекта (бэкап в `<file>.json.bak`);
- восстанавливает `profile.json` для осиротевшей папки;
- синхронизирует `id` / `model_id` с именем файла;
- воссоздаёт досье, если нашлась переписка без анкеты;
- уводит непривязанные вложения в `attachments/_orphans/`;
- перестраивает `global_index.json`.

---

## Запуск

```bash
npm install
npm run desktop:dev      # десктоп
npm run android:init     # один раз, нужен Android SDK + NDK
npm run android:dev      # телефон / эмулятор
```

Сборка релиза: `npm run desktop:build`, `npm run android:build`.

### Требования

- Node 20+, Rust 1.77+
- Linux: `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libxdo-dev libssl-dev`
- Android: JDK 17, Android SDK 34, NDK 27, `ANDROID_HOME` + `NDK_HOME`

### Настройка провайдера

Топбар → `🔑`:

1. выбери провайдера (Gemini или OpenAI-совместимый);
2. base URL, модель, версию API, температуру, доп. заголовки (для OpenRouter: `HTTP-Referer`, `X-Title`);
3. добавь один или несколько ключей — они попадут в пул ротации;
4. «Проверить связь» делает дешёвый ping-запрос.

Примеры base URL: `https://generativelanguage.googleapis.com` (Gemini),
`https://openrouter.ai/api/v1`, `https://api.deepseek.com/v1`, `http://localhost:11434/v1` (Ollama).

---

## Сборки в CI

`.github/workflows/build.yml` запускается **на каждый push** и собирает:

| Задание | Артефакт |
| --- | --- |
| `linux-x86_64` | `.deb`, `.AppImage`, `.rpm` |
| `windows-x86_64` | `.msi`, `.exe` (NSIS) |
| `macos-aarch64` / `macos-x86_64` | `.dmg`, `.app.tar.gz` |
| `android-apk` | `.apk` (debug-подпись, ставится на телефон) |

Перед сборкой прогоняются `cargo fmt`, `cargo clippy -D warnings`, `cargo test` и `tsc + vite build`.

`.github/workflows/release.yml` по тегу `v*` собирает релизные бандлы, публикует GitHub Release
и прикладывает подписанные APK. Ключ подписи берётся из секретов
`ANDROID_KEYSTORE_BASE64`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`;
если их нет — APK подписывается одноразовым CI-ключом (для теста, не для стора).

Все раннеры бесплатны для публичных репозиториев.

---

## Безопасность и приватность

- Ключи лежат в `secrets.json` рядом с данными (`chmod 600` на unix) и никогда не уходят никуда, кроме выбранного LLM-провайдера.
- Телеметрии нет.
- CSP запрещает всё, кроме собственных ресурсов и картинок по https.
- Тексты переписки уходят только в тот провайдер, который ты настроил.

## Лицензия

MIT.
