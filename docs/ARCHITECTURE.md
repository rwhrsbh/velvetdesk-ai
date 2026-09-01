# VelvetDesk AI — системный дизайн

## 1. Слои

```
┌────────────────────────────────────────────────────────────────────────┐
│                   FRONTEND (Webview / Apple HIG UI)                    │
│  [Profile Rail] ◄──► [Agent Copilot Studio] ◄──► [Target Men CRM Rail] │
└──────────────────────────────────┬─────────────────────────────────────┘
                                   │ IPC (Tauri commands / events)
┌──────────────────────────────────▼─────────────────────────────────────┐
│                       TAURI 2 / RUST CORE                              │
│ ┌──────────────────────┐ ┌────────────────────┐ ┌────────────────────┐ │
│ │ storage::Scope       │ │ llm::KeyPool +     │ │ agent::run +       │ │
│ │ (sandbox, atomic io) │ │ провайдеры         │ │ doctor (валидация) │ │
│ └──────────────────────┘ └────────────────────┘ └────────────────────┘ │
│  app_data/profiles/<model_id>/{profile,men,chats,agent_log}.json       │
└────────────────────────────────────────────────────────────────────────┘
```

| Модуль | Файл | Ответственность |
| --- | --- | --- |
| Хранилище | `src-tauri/src/storage.rs` | пути, `Scope`, атомарная запись, глобальный индекс, кросс-поиск |
| Модели | `src-tauri/src/models.rs` | `Profile`, `Man`, `ChatThread`, `AgentLog`, компактные досье для промптов |
| Конфиг | `src-tauri/src/config.rs` | `Settings`, `Secrets`, провайдеры, режимы, уровни доступа |
| LLM | `src-tauri/src/llm/` | абстракция чата, `KeyPool`, Gemini, OpenAI-совместимый |
| Агент | `src-tauri/src/agent/` | промпты, инструменты, планировщик мутаций, AUTO/ACT/MEMORIZE, мастер-роутер |
| Доктор | `src-tauri/src/doctor.rs` | ремонт JSON, проверка схем и связей |
| IPC | `src-tauri/src/commands.rs` | 33 команды для фронта |
| Состояние | `src-tauri/src/state.rs` | настройки, пулы ключей, очередь подтверждений |

## 2. Поток одного запуска агента

```
UI                 commands::run_agent        agent::run              tools::execute
 │  invoke run_agent  │                          │                        │
 ├───────────────────►│  берёт провайдер + пул   │                        │
 │                    ├─────────────────────────►│ строит system prompt   │
 │                    │                          │ (персона + досье)      │
 │                    │                          ├──── LLM вызов ────────►│
 │  события шагов ◄───┴──── velvetdesk://agent ──┤                        │
 │                                               │ tool_calls ───────────►│ plan_mutation
 │                                               │                        │ ├ разрешено → commit
 │                                               │                        │ └ нет → PendingAction
 │  RunOutput (reply, steps, pending, usage) ◄───┘                        │
```

AUTO повторяет цикл до `max_tool_turns`. ACT и MEMORIZE делают ровно один вызов
с `force_json` и прогоняют `memory_patch` через те же инструменты — значит правила
безопасности и очередь подтверждений работают одинаково во всех режимах.

## 3. Инструменты агента

| Инструмент | Риск | Что делает |
| --- | --- | --- |
| `list_men`, `get_man`, `get_profile`, `get_chat`, `search_scope` | read | чтение внутри песочницы |
| `create_man`, `update_man`, `add_man_fact`, `add_man_note`, `add_gift`, `add_tags`, `append_chat`, `update_profile` | write | аддитивные изменения CRM |
| `delete_man`, `replace_profile_prompt` | destructive | удаление и перезапись персоны |

`plan_mutation` считает «до / после» без записи — отсюда берётся diff-превью для
режимов Ask/Safe. `commit` — единственное место, где данные попадают на диск.

## 4. События

Ядро шлёт `velvetdesk://agent` с полезной нагрузкой:

- `{ kind: "step", step: RunStep }` — инструмент выполнен или поставлен в очередь;
- `{ kind: "llm_retry", key_index, verdict, message }` — ключ отвалился, идёт ротация;
- `{ kind: "llm_wait", message }` — все ключи в кулдауне, ждём.

## 5. Тесты

`cargo test` покрывает: отказ traversal-путей, round-trip хранилища и индекса,
round-robin и кулдауны пула, парсинг ответов Gemini и OpenAI, извлечение JSON из
болтливого ответа, ремонт битого JSON, доктора, политику безопасности инструментов
и применение `memory_patch`.
