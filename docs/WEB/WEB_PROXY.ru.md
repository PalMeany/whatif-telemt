# WEB-режим прокси

[English](WEB_PROXY.en.md) | [Русский](WEB_PROXY.ru.md) | [Deutsch](WEB_PROXY.de.md)

WEB-режим переносит обычные MTProxy-потоки через ограниченный HTTPS long-poll transport, совместимый с типом прокси `WEB` в Telegram Desktop. В первой реализации Telemt не терминирует TLS: публичный сертификат обслуживает NGINX или HAProxy, который передаёт обычный HTTP/1.1 на приватный listener Telemt.

> [!IMPORTANT]
>
> WEB-режим реализован и настраивается в текущем дереве исходного кода. Для первого развёртывания нужен бинарный файл, собранный из ревизии с этой реализацией, и перезапуск процесса Telemt. Готовый пакет можно использовать только после проверки, что он содержит эту ревизию. Сквозная проверка с целевой сборкой Telegram Desktop и реальным публичным TLS endpoint остаётся обязательным приёмочным шагом оператора.

## Путь трафика

```text
Telegram Desktop
    | HTTPS :443
    v
NGINX или HAProxy (TLS termination, канонические Host и X-Forwarded-For)
    | обычный HTTP/1.1 в приватной сети
    v
WEB-listener Telemt
    |-- аутентифицированный carrier --> bounded logical MTProxy relays --> Telegram
    `-- обычный или некорректный запрос --> настроенный decoy site
```

Направляйте в Telemt весь публичный vhost. Если TLS-терминатор будет выделять только известные carrier paths, поведение обычных и аутентифицированных запросов станет наблюдаемо различным, а decoy policy Telemt будет обойдена.

## Поддерживаемый контракт клиента

- Публичный endpoint всегда имеет вид `https://HOST:443`.
- Поддерживаются 16-байтовые MTProxy-секреты `plain` и `dd`. FakeTLS-секреты `ee` в WEB-режиме не поддерживаются.
- Первая версия carrier использует сериализованные HTTPS uplink-запросы и HTTPS long polling. WebSocket- и lane-carriers не анонсируются.
- Capability, bootstrap и session credentials — отдельные значения с ограниченным сроком жизни. Carrier credentials считаются секретами и не должны попадать в access logs.
- Внутренняя MTProxy-аутентификация ограничена пользователем и режимом секрета, выбранными профилем vhost. Некорректный внутренний handshake закрывает только свой logical stream и никогда не попадает в TCP masking path.

В WEB-ссылках Telegram Desktop нет порта, потому что клиент требует порт 443:

```text
tg://webproxy?server=proxy.example.com&secret=0123456789abcdef0123456789abcdef
tg://webproxy?server=proxy.example.com&secret=dd0123456789abcdef0123456789abcdef
```

Telemt печатает ссылки для WEB-профилей, выбранных в `[general.links].show`, через существующий log target `telemt::links`.

## Предварительные требования

- Отдельный публичный FQDN и действующий TLS-сертификат на NGINX или HAProxy.
- Стабильный публичный IP этого hostname. В `public_addr` должен быть указан именно этот конкретный IP с портом 443, поскольку адрес участвует во внутреннем destination tuple relay.
- Приватный или loopback HTTP-путь от TLS-терминатора до Telemt.
- Обычный decoy site: приватный HTTP origin либо immutable snapshot локального каталога.
- Совместимая сборка Telegram Desktop с типом прокси `WEB`.

Если один hostname обслуживается одновременно по IPv4 и IPv6, в первой реализации используйте отдельные hostname или отдельные экземпляры Telemt. Forwarded client address и `public_addr` должны принадлежать одному семейству IP.

## Минимальная конфигурация Telemt

В примере WEB-listener остаётся на loopback, а decoy использует приватный HTTP origin:

```toml
[general.links]
show = ["web-user"]

[access.users]
web-user = "0123456789abcdef0123456789abcdef"

[[server.listeners]]
ip = "127.0.0.1"
port = 18080
transport = "web"
proxy_protocol = false
web_client_ip_source = "x_forwarded_for"
web_trusted_proxy_cidrs = ["127.0.0.1/32"]

[web]
enabled = true

[[web.vhosts]]
host = "proxy.example.com"
public_addr = "203.0.113.10:443"

[web.vhosts.decoy]
mode = "http_upstream"
upstream = "http://127.0.0.1:18081"

[[web.vhosts.profiles]]
user = "web-user"
secret_mode = "dd"
max_sessions = 8
max_streams = 512
max_streams_per_session = 64
```

Для WEB-listener обязательны `proxy_protocol = false` и `reuse_allow = false`. В нём нельзя использовать `client_mss`, `synlimit`, `announce` и `announce_ip`. Массив `web_trusted_proxy_cidrs` должен быть непустым и содержать только непосредственные адреса NGINX или HAProxy; сети `/0` запрещены.

HTTP decoy origin должен быть loopback, link-local или private IP literal. Для обычных запросов Telemt сохраняет method, path, query, headers, streamed body, response status, headers и body, удаляя hop-by-hop headers. Перед отправкой некорректного carrier-запроса в decoy Telemt удаляет из него carrier credentials и body.

Вместо origin можно использовать immutable snapshot статического сайта:

```toml
[web.vhosts.decoy]
mode = "static_directory"
directory = "/var/lib/telemt/public"
index = "index.html"
```

Статические файлы читаются при запуске и успешном reload конфигурации. Число элементов, размер одного файла и общий размер snapshot ограничены `[web.limits]`. Symlinks и пути с выходом из настроенного каталога запрещены. Не изменяйте каталог одновременно с построением snapshot в Telemt.

Все WEB-ключи и defaults перечислены в [справочнике конфигурации](../Config_params/CONFIG_PARAMS.ru.md#web).

## Терминация TLS на NGINX

```nginx
upstream telemt_web {
    server 127.0.0.1:18080;
    keepalive 64;
}

server {
    listen 443 ssl;
    server_name proxy.example.com;
    access_log off;

    ssl_certificate     /etc/letsencrypt/live/proxy.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/proxy.example.com/privkey.pem;

    client_max_body_size 2m;

    location / {
        proxy_pass http://telemt_web;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header Connection "";

        proxy_connect_timeout 5s;
        proxy_send_timeout 35s;
        proxy_read_timeout 35s;
        proxy_request_buffering off;
        proxy_buffering off;
        proxy_next_upstream off;
    }
}
```

`client_max_body_size` должен быть не меньше `web.limits.max_body_bytes`. Значения `proxy_read_timeout` и `proxy_send_timeout` должны превышать `web.timeouts.long_poll_secs`, по умолчанию равный 25 секундам. Перезаписывайте `X-Forwarded-For`, а не дополняйте его. Не включайте upstream retries: byte-identical retry выполняет сам bridge по своему sequence protocol.

## Терминация TLS на HAProxy

```haproxy
frontend public_https
    mode http
    no log
    bind :443 ssl crt /etc/haproxy/certs/proxy.example.com.pem alpn h2,http/1.1
    acl telemt_web_host hdr(host) -i proxy.example.com proxy.example.com:443
    use_backend telemt_web if telemt_web_host

backend telemt_web
    mode http
    option http-keep-alive
    retries 0
    timeout connect 5s
    timeout server 35s
    http-request set-header Host proxy.example.com
    http-request del-header X-Forwarded-For
    http-request set-header X-Forwarded-For %[src]
    server telemt_web_1 127.0.0.1:18080 check
```

Во frontend или секции `defaults` также задайте `timeout client` выше long-poll deadline. Не переписывайте path, raw query, body и carrier headers `Authorization`, `Content-Type`, `X-Up-Seq`, `X-Down-Cursor`.

## Lifecycle и reload

| Конфигурация | Поведение runtime |
| --- | --- |
| Состав WEB-listeners, bind address и trust policy | Принадлежат процессу; перезапустите Telemt. |
| Любое значение `[web.limits]` | Process-owned контракт памяти и ресурсов; перезапустите Telemt. |
| `web.enabled`, timeouts, vhosts, profiles и decoys | Применяются config watcher или runtime generation reload. |
| Существующие HTTP connections и WEB sessions | Сохраняют лимиты и deadlines своего момента создания; новые logical streams используют активное runtime generation. |
| Завершение процесса | Использует последнее применённое значение `web.timeouts.shutdown_secs`. |

Каждый logical stream сохраняет client IP своей сессии и владеет уникальным в пределах процесса ненулевым synthetic source port до завершения relay. Это сохраняет один стабильный непересекающийся source/destination tuple для Direct и Middle-End KDF routing.

## Управление через API

Управление через API доступно, но намеренно ограничено. Отдельных endpoint `/v1/web` и WEB-specific runtime statistics endpoint сейчас нет.

| Операция | Поддержка API |
| --- | --- |
| Чтение или изменение `[web]`, vhosts, profiles, decoys, timeouts или limits | Нет. `GET /v1/config` не возвращает `[web]`; `PATCH /v1/config` отвечает `400 section_not_editable` на ключ `web`. |
| Сохранение `server.listeners` | Да, через `PATCH /v1/config`, но изменённый WEB-listener остаётся deferred до перезапуска процесса. |
| Применение WEB-конфигурации, изменённой вне API | Да, через `POST /v1/system/reload` с последующей проверкой статуса операции. |
| Управление `[access.users]` | Да, через `/v1/users`. Создание пользователя не создаёт WEB-профиль. |
| Отзыв отдельного пользователя | Да. `/v1/users/{username}/disable` немедленно обновляет admission и завершает активные сессии пользователя. |

Привяжите API к loopback, оставьте узким whitelist непосредственных peers, настройте точное значение authorization header и используйте `read_only = false` только там, где нужны мутации:

```toml
[server.api]
enabled = true
listen = "127.0.0.1:9091"
whitelist = ["127.0.0.0/8"]
auth_header = "Bearer replace-with-a-random-control-token"
read_only = false
```

API whitelist проверяет непосредственный TCP peer и не доверяет `X-Forwarded-For`. Изменения самой секции `[server.api]` требуют перезапуска процесса.

После атомарного изменения TOML-файла администратором или системой управления конфигурацией задайте в `TELEMT_API_AUTH` точное значение `auth_header` и отправьте наблюдаемый generation reload:

```bash
curl -sS -X POST http://127.0.0.1:9091/v1/system/reload \
  -H "Authorization: ${TELEMT_API_AUTH}" \
  -H 'Content-Type: application/json' \
  -d '{"mode":"drain","timeout_secs":30,"failure_policy":"rollback"}'

# Use data.reload_id from the response.
curl -sS http://127.0.0.1:9091/v1/system/reload/RELOAD_ID \
  -H "Authorization: ${TELEMT_API_AUTH}"
```

Терминальный статус `succeeded` подтверждает активацию runtime. Если `deferred_process_fields` содержит `server.listeners` или `web.limits`, файл валиден и сохранён, но эти настройки всё ещё требуют перезапуска Telemt.

Операции с access users используют существующие endpoints, например:

```bash
curl -sS -X POST http://127.0.0.1:9091/v1/users/web-user/disable \
  -H "Authorization: ${TELEMT_API_AUTH}"

curl -sS -X POST http://127.0.0.1:9091/v1/users/web-user/rotate-secret \
  -H "Authorization: ${TELEMT_API_AUTH}" \
  -H 'Content-Type: application/json' \
  -d '{}'
```

После ротации секрета config watcher перестраивает WEB capabilities. Users API возвращает секрет, но не URL `tg://webproxy`; соберите ссылку из настроенного hostname и представления `plain` или `dd` соответствующего профиля. Перед удалением пользователя, на которого ссылается WEB-профиль, сначала удалите и примените этот профиль, чтобы итоговая конфигурация оставалась валидной.

Полный контракт запросов, revisions, ошибок и всех user endpoints приведён в [документации Control API](../Architecture/API/API.md).

## Инварианты развёртывания

- Никогда не публикуйте plain HTTP WEB-listener в недоверенной сети. Закрепите это host firewall rules, даже если listener использует loopback.
- Отключите логирование request target и authorization на TLS-терминаторе либо используйте проверенный формат с редактированием. Raw queries содержат bridge capabilities, а `Authorization` — bootstrap или session bearer credentials.
- Сохраняйте один стабильный публичный адрес на vhost. Если DNS возвращает несколько ingress addresses, каждый deployment должен использовать адрес своего внешнего пути.
- Bootstrap- и session-registries локальны для процесса. Для multi-process или multi-host upstream pool нужна affinity всего vhost: bridge GET, создание сессии, uplink, downlink и DELETE. Одному процессу Telemt дополнительная affinity не нужна.
- Decoy входит в anti-probing contract. До распространения ссылок проверьте через публичный TLS endpoint его обычный ответ 404 и response timing.

## Первичная проверка

1. Запустите пересобранный Telemt с WEB-конфигурацией и убедитесь, что приватный listener привязан.
2. Через публичный TLS endpoint проверьте, что `GET /`, неизвестный path и некорректный query `bridge` возвращают настроенный decoy site.
3. Убедитесь, что Telemt получает один канонический адрес `X-Forwarded-For` и `Host: proxy.example.com` либо `Host: proxy.example.com:443`.
4. Импортируйте напечатанную ссылку `tg://webproxy` в целевую сборку Telegram Desktop и установите соединение через прокси.
5. Проверьте reconnect и как минимум один long poll длительнее 25 секунд, чтобы frontend timeouts не обрывали carrier.
6. Проверяйте лимиты пользователя и logical MTProxy connections по logical-stream counters, а не по числу HTTP connections.

## Диагностика

| Симптом | Что проверить |
| --- | --- |
| WEB-конфигурация валидна на диске, но поведение listener’а не изменилось | Проверьте `deferred_process_fields`; listener и `[web.limits]` требуют перезапуска. |
| Carrier-запросы попадают в decoy | Проверьте точный vhost, secret mode ссылки, CIDR непосредственного proxy и единственное каноническое значение `X-Forwarded-For`. |
| Long polls разрываются через фиксированный интервал | Поднимите client, server, send и read timeouts NGINX/HAProxy выше `web.timeouts.long_poll_secs`. |
| Telegram Desktop отклоняет ссылку | Не указывайте порт, используйте валидный FQDN, внешний порт 443 и только `plain` или `dd`. |
| Один узел работает, но load-balanced pool нестабилен | Настройте affinity всего vhost: WEB credential registries локальны для процесса. |
