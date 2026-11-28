# Mapa de capacidades

Qué puede probar Nadir, de un vistazo: por protocolo, por mutador y por modo. Cada fila enlaza a la página que lo explica en detalle. Cuando se agrega un mutador o un modo, esta página se actualiza en el mismo cambio.

## 1. Protocolos

| Protocolo | Qué prueba | Mutadores | Reglas implícitas | Qué se observa |
|---|---|---|---|---|
| [`http`](cases.md) | Una API HTTP descrita en OpenAPI, como un flujo de varios pasos | `json`, `http` (alias `headers`), `body`, `file`, `file_fuzz`, renombrar claves, `examples` | `schema:rejects-invalid`, `example:<id>`, `fuzz:key:<clave>`, `body:handled` | `response.status`, `response.header.*`, `response.body.*` |
| [`http_raw`](http-raw.md) | El mensaje HTTP en sí: bytes mal formados que el cliente normal no envía | `malform` | `wire:handled` | El status, o que cerró la conexión |
| [`cli`](cli.md) | Un comando local, o un servicio que arranca y se sondea (`service`) | `envar`, `envfile` | `startup:refuses`, `startup:defaults`, `envfile:refuses` | `process.exit_code`, `stdout`, `stderr`, `timed_out`, `ready`, `stopped_code` |

Común a los tres: [`print` y `debug`](output.md), [`hidden`](experiment.md#hidden-qué-se-oculta), [oráculos](oracles.md), crashes al momento en `artifacts/crashes/` y [`replay`](output.md#resultados-y-reproducción).

## 2. Mutadores por protocolo

**Límite de rutas aleatorias:** en `protocol: http`, los valores aleatorios de parámetros de ruta se recortan a [`protocols.http.max_path_chars`](experiment.md#estructura-completa) caracteres (4096 por defecto), para que la URL quepa en el cliente. `http_raw` no tiene ese límite.

**Tipo de caso:** *enumerado* significa un caso fijo por valor, que corre siempre y cuenta contra `max_cases`. *Aleatorio* significa que solo corre con `strategy: fuzz`, cuenta contra `max_execs` y se repite desde la semilla.

**Orden de la enumeración:** caso base → variantes de `json` y `http` (catálogos, `values`, `path`) → `examples` → `file` / `file_fuzz` → `envfile` → `malform` → `envar` → fase aleatoria. Si `max_cases` se agota, los casos enumerados que faltan **no corren**: con un presupuesto chico, lo primero que se pierde es `malform` y `envar`. La fase aleatoria sigue igual, así que `Stop reason` puede decir `MaxExecsReached` aunque la enumeración se haya cortado; por eso Nadir lo avisa aparte: en el progreso (`Enumeration cases: 40/185 · max_cases runs 99`) y en el resumen (`Enumeration cases: 99 of 185 (max_cases reached: 86 enumerated cases did not run; raise max_cases)`).

### `protocol: http`

| Mutador | Qué cambia | Opciones | Tipo de caso | Lo juzga |
|---|---|---|---|---|
| [`json`](cases.md#mutators-catálogos-de-valores) | Valores de campos del cuerpo JSON | `fields`, `mode`, `values`, `length` (un campo), `path` | Enumerado (catálogos, `values`, `path`) y aleatorio (generadores; con `fuzz`, todos sus campos) | Tus oráculos; con `mode: schema`, también `schema:rejects-invalid` |
| [`http`](cases.md#http-cabeceras-y-parámetros-de-ruta) | Cabeceras (`header.X`) y parámetros de ruta (`path.x`); reemplaza el `bind` | `fields`, `mode`, `values`, `length` (un campo), `path` | Igual que `json` | Tus oráculos; con `mode: schema`, también `schema:rejects-invalid` |
| `headers` | Alias anterior de `http` para cabeceras | `fields`, `values` | Enumerado | Tus oráculos |
| [`body`](cases.md#use-body-caracteres-aleatorios-en-el-cuerpo-json) | El texto del cuerpo JSON ya serializado: inserta, borra, reemplaza o duplica caracteres | `mode` (`alpha`, `alphanum`, `num`, `raw`), `edits: {max}` | Aleatorio | `body:handled` |
| [`file`](cases.md#corpus-de-texto-por-líneas) | Un campo del cuerpo, desde las líneas de un archivo, en ciclo hasta `repetitions` (solo letras, dígitos y espacios) | `fields` (uno), `path`, `repetitions` | Enumerado | Tus oráculos |
| [`file_fuzz`](cases.md#corpus-de-texto-por-líneas) | Lo mismo, usando cada línea como semilla de un generador | `fields` (uno), `path`, `repetitions`, `mode`, `seed`, `length` | Enumerado, con semilla fija | Tus oráculos |
| [Renombrar claves](cases.md#nombres-de-claves-y-sintaxis-json) | Nombres de propiedades del cuerpo (`use: json` con `keys`) | `keys`, `mode` (`alpha`, `alphanum`, `raw`), o `rename_to` con `expect_status` | Aleatorio (`mode`) o enumerado (`rename_to`) | Tus oráculos; con `expect_status`, también `fuzz:key:<clave>` o `example:<id>` |
| [`examples`](cases.md#examples-casos-concretos-con-status-esperado) | Un valor concreto en un campo, o un JSON roto (`syntax`) | `field` y `value`, o `syntax`; `expect_status` | Enumerado | `example:<id>` (el status exacto) |

### `protocol: http_raw`

| Mutador | Qué cambia | Opciones | Tipo de caso | Lo juzga |
|---|---|---|---|---|
| [`malform`](http-raw.md) | El mensaje HTTP: método, versión, ruta, cabeceras, `Content-Length`, *chunked*, terminación | `mode: catalog`, `only`, `sizes` (`uri`, `header`, `headers`) | Enumerado (28 malformaciones) | `wire:handled` |

Catálogo de `malform`: `bad_method`, `lowercase_method`, `bad_version`, `missing_version`, `double_space`, `huge_uri`, `nul_in_path`, `cr_in_path`, `crlf_injection`, `bare_lf`, `header_without_colon`, `space_before_colon`, `folded_header`, `many_headers`, `huge_header`, `missing_host`, `duplicate_host`, `short_length`, `long_length`, `negative_length`, `non_numeric_length`, `conflicting_lengths`, `length_and_chunked`, `bad_chunk_size`, `unterminated_chunked`, `trailing_bytes`, `no_header_terminator`, `truncated_request`.

### `protocol: cli`

| Mutador | Qué cambia | Opciones | Tipo de caso | Lo juzga |
|---|---|---|---|---|
| [`envar`](cli.md#use-envar-variar-variables-de-entorno) | Una variable de entorno por caso (o la quita) | `variables` (lista, mapa, `all` o `spec`), `mode`, `values`/`value`, `path`, `length` | Enumerado (catálogos, `values`, `path`, `absent`) y aleatorio (generadores) | Tus oráculos |
| [`envar`, `mode: spec`](cli.md#mode-spec-valores-que-violan-la-especificación) | Cada variable de un archivo de especificación, con los valores que la documentación prohíbe | `spec`, `only`; en la especificación: tipo, `used_when`, `skip`, `required`, `max_mode`, `max_size` | Enumerado | `startup:refuses`; el caso `unset`, `startup:refuses` o `startup:defaults` |
| [`envfile`](cli.md#use-envfile-romper-el-archivo-env) | El archivo `.env` que copia `workdir`, roto de una forma por caso | `file`, `limits`, `prohibited`, `only` | Enumerado (14 casos, más `prohibited` y `limits`) | `envfile:refuses` |

Para arrancar el programa en cada caso: [`env_file`, `workdir` y `service`](cli.md#env_file-workdir-y-service-arrancar-un-servicio-en-cada-caso).

## 3. Modos por mutador

La regla es que todo modo funcione en todo mutador. Si un transporte no puede llevar ciertos caracteres, el modo se mantiene y esos caracteres se filtran.

| Modo | `json` | `http` | `envar` | Notas |
|---|---|---|---|---|
| `edge_cases` | ✓ | ✓ | ✓ | `""`, `"a"×200`, `"A"×200`, `"测试"`, `"RESERVED"` |
| `unconstrained` | ✓ | ✓ (cabeceras sin `nul_byte`) | ✓ (sin `nul_byte`; más `non_unicode`) | Incluye `</script>`, `' OR '1'='1`, `${}`, 65 536 caracteres |
| `alpha` / `alphanum` / `num` / `raw` | ✓ | ✓ (`raw` sin controles en cabeceras) | ✓ (`raw` sin NUL) | Aleatorios; con `length: {reference, max}` |
| `values` | ✓ (después del catálogo) | ✓ (`mode: values`: solo la lista) | ✓ | Enumerados |
| `path` | ✓ | ✓ (cabeceras sin líneas con controles) | ✓ | Un caso por línea; replay se niega si el archivo cambió |
| `absent` | — | ✓ (sin la cabecera o con el segmento vacío) | ✓ (sin la variable) | En `json`, quitar una clave lo cubre renombrar claves |
| Catálogo desde un documento | `schema` (OpenAPI) | `schema` (OpenAPI) | `spec` (archivo de especificación) | Valores fuera del esquema documentado |

**Hueco conocido:** renombrar claves acepta `alpha`, `alphanum` y `raw`, pero no `num`.

**Sin equivalente en los demás, a propósito:**
- `malform`, `envfile` y `body` rompen el mensaje, el archivo o el texto del cuerpo completo, no un valor (`body` usa los alfabetos de los generadores);
- `non_unicode` solo existe en `envar`, porque JSON y las cabeceras no pueden llevar bytes que no son UTF-8.

## 4. Reglas implícitas

Se agregan solas a los casos de su tipo. Las tres primeras se suman a tus oráculos. Con `body:handled`, `wire:handled`, `startup:refuses`, `startup:defaults` y `envfile:refuses`, en cambio, tus oráculos del paso solo juzgan el caso base: describen el programa bien configurado, no el roto.

| Regla | Casos que juzga | Exige | Falla con |
|---|---|---|---|
| `schema:rejects-invalid` | `mode: schema` (salvo `at_max_length`) | Un status ≥ 400 | Status 2xx o 3xx |
| `example:<id>` | Cada ejemplo, y los renombres fijos (`rename_to`) | El `expect_status` exacto | Otro status |
| `fuzz:key:<clave>` | Los renombres generados (`keys` con `mode`) que llevan `expect_status` | El `expect_status` exacto | Otro status |
| `body:handled` | `body` | Una respuesta sin 5xx antes del timeout (cerrar la conexión falla) | `ServerError`, `Unresponsive` |
| `wire:handled` | `malform` | Una respuesta sin 5xx, o la conexión cerrada, antes del timeout | `ServerError`, `Unresponsive` |
| `startup:refuses` | `mode: spec` (y `unset` con `required: true`) | Salir con código distinto de 0 antes de `ready`, nombrando la variable y sin mostrar el valor | `AcceptedInvalidConfig`, `Crash`, `Unresponsive`, `Security`, `Assertion` |
| `startup:defaults` | El `unset` de una variable opcional de `mode: spec` | Arrancar con el valor por defecto (`ready`, o salir con 0) | `Assertion`, `Crash`, `Unresponsive` |
| `envfile:refuses` | `envfile` | Negarse a arrancar, decir la línea (o el archivo) y no mostrar el valor | `AcceptedInvalidConfig`, `Crash`, `Unresponsive`, `Security`, `Assertion` |

## 5. Oráculos y selectores

| Oráculo | Comprueba | Protocolos |
|---|---|---|
| [`openapi`](oracles.md) | Status no 5xx y documentado en el OpenAPI | `http` |
| [`openapi_schema`](oracles.md) | El cuerpo cumple el JSON Schema de la respuesta | `http` |
| [`record_absent`](oracles.md) | Que un recurso no exista | `http` |
| [`assert`](oracles.md) | `equals`, `one_of`, `exists` o `absent` sobre un selector, con `when` opcional | Todos |

| Selector | Protocolos |
|---|---|
| `response.status`, `response.header.<Nombre>`, `response.body.<ruta>` | `http`, `http_raw` |
| `steps.<id>.response.<…>` (un paso anterior) | `http` |
| `process.exit_code`, `process.stdout`, `process.stderr`, `process.timed_out` | `cli` |
| `process.ready`, `process.stopped_code` | `cli` con `service` |

## 6. Registros y `replay`

Cada caso guardado se repite con `nadir replay`. El tipo de caso decide la versión del registro y desde dónde se reconstruye:

| Versión | Tipo de caso | Se reconstruye desde |
|---|---|---|
| 1 | Registros antiguos | Su posición en el catálogo |
| 2 | Caso base, variante de un mutador, ejemplo con `field`/`value`, caso aleatorio (campos, renombres generados, `body`) | El nombre del valor en el catálogo, el ejemplo, o la semilla y la iteración |
| 3 | `file` / `file_fuzz` | La línea, la iteración y la huella del archivo |
| 4 | Ejemplo que cambia el JSON entero (`syntax`, renombre fijo) | El ejemplo guardado |
| 5 | `malform` | El nombre de la malformación |
| 6 | `envar` | La variable y el caso (`value`, `line`, `spec`, `catalog`, `unset`) |
| 7 | `envfile` | El nombre del caso y el archivo copiado |

`replay` se niega si el YAML del experimento cambió o si cambió el archivo de un caso de `path`, `file` o `file_fuzz`. Si cambió el OpenAPI, repite el caso mientras el campo guardado siga existiendo y su valor encaje en la operación, y se niega si no.

## 7. Diagnósticos fuera de los oráculos

No todo lo que sale mal es un hallazgo de una regla. Esto es lo que Nadir registra aparte:

| Resultado | Qué significa | Dónde queda |
|---|---|---|
| Hallazgo (regla que falla) | Un oráculo o una regla implícita falló | `artifacts/crashes/`, se repite con `replay` |
| `process_exit` | El servicio que arrancó `target` terminó durante la ejecución | `artifacts/crashes/` |
| `transport_error`: `connection_failed`, `timeout`, `reset`, `connection_closed`, `body_read_failed` | La petición salió y la conexión falló | `artifacts/transport-errors/`, una captura por paso, motivo y fase |
| `transport_error (unsendable)` | El cliente HTTP no pudo armar la petición (URL de más de 65 534 bytes, cabecera inválida): no llegó nada al servicio | Solo en el caso; los oráculos del paso quedan `inconclusive` |
| `invalid_url`, `non_loopback` | La configuración apunta a una URL inválida o fuera de loopback | Error de preparación |
| Enumeración cortada por `max_cases` | Quedaron casos enumerados sin correr (los últimos del orden: `malform`, `envar`) | Solo en la salida: el progreso (`Enumeration cases: 40/185 · max_cases runs 99`) y el resumen (`99 of 185 (max_cases reached: …)`); no cambia el código de salida |

En los casos que juzgan `wire:handled` y `body:handled`, un timeout o un cierre de conexión es parte de la regla (falla o pasa según cada una), no un error de transporte. Detalles en [`output.md`](output.md#diagnósticos-fuera-de-los-oráculos).
