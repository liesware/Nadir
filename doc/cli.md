# `protocol: cli`: probar un comando local

`protocol: cli` ejecuta un programa y verifica su código de salida, `stdout` y `stderr` con los mismos oráculos del resto de pasos. El comando recibe un entorno aislado: solo las variables de `env_file` y `env` (ni siquiera `PATH` si no lo declaras). `stdin` está cerrado y los flujos de salida se capturan hasta 64 KiB cada uno.

Los valores de `env` admiten referencias: `${env.NOMBRE}` (con `:-valor` por defecto), `${case.id}` y `${target.port}`. Así un secreto no se escribe en el YAML: `VECTIS_APIKEY: "${env.NADIR_VECTIS_API_KEY}"` lee la variable de Nadir y, si está en [`hidden`](experiment.md#hidden-qué-se-oculta), su valor sale como `[hidden]` en la evidencia. `${steps.<id>.outputs.<nombre>}` no está disponible en pasos CLI.

```yaml
steps:
  - id: render
    protocol: cli
    command: ["./fixtures/demo-cli", "render"]
    env:
      APP_LABEL: "demo"
      APP_PROFILE: "standard"
      APP_OUTPUT_FORMAT: "json"
      APP_LOCALE: "en-US"
    cases:
      mutators:
        - use: envar
          variables: [APP_LABEL, APP_PROFILE]
          mode: alphanum
        - use: envar
          variables:
            APP_OUTPUT_FORMAT:
              values: ["json", "text"]
            APP_LOCALE:
              value: "es-MX"
    oracles:
      - id: command-succeeds
        use: assert
        select: process.exit_code
        expect:
          equals: 0
```

**Configuración del protocolo:**

```yaml
protocols:
  cli:
    timeout_ms: 5000   # por defecto 5000; si se agota, el proceso se mata y process.timed_out es true
```

El bloque `protocols.cli` es opcional: sin él, los pasos CLI usan el timeout por defecto.

**Selectores de observación** para pasos CLI:

| Selector | Valor |
|---|---|
| `process.exit_code` | Código de salida del proceso (entero). `-1` cuando no hay código, por ejemplo si el proceso terminó por señal, venció el plazo o Nadir no pudo obtener su estado. |
| `process.stdout` | Salida estándar completa (texto). |
| `process.stderr` | Salida de error completa (texto). |
| `process.timed_out` | `true` si se agotó `timeout_ms` (o `service.ready.timeout_ms`) y el proceso fue matado; `false` en caso contrario. |
| `process.ready` | Solo con `service`: `true` si la URL de `ready` respondió 2xx. |
| `process.stopped_code` | Solo con `service`: el código de salida después de que Nadir detuvo el servicio listo. |

**Resultado de la ejecución:**
- El comando se resuelve relativo al directorio del experimento. El proceso arranca en ese directorio, o en la carpeta de trabajo de `workdir`.
- Cada caso copia el `env` base y reemplaza **una sola variable** con el valor de la variante.
- Un código de salida distinto de cero es un resultado evaluable por los oráculos. Un fallo al lanzar el proceso o un timeout deja el caso inconcluso. Si un oráculo confirma un hallazgo, la ejecución se informa como fallida.
- Si el proceso termina por una señal o falla la consulta de su estado, `debug` indica la causa y el JSON del caso la guarda en `process_diagnostic`. Un error de consulta antes de confirmar disponibilidad o de vencer el plazo deja el caso inconcluso.
- Los valores marcados como `secret` en `outputs` o listados en [`hidden`](experiment.md#hidden-qué-se-oculta) se redactan de la salida.

## `env_file`, `workdir` y `service`: arrancar un servicio en cada caso

```yaml
- id: serve
  protocol: cli
  command: ["./vectis", "serve"]
  env_file: ./.env                    # entorno base
  env:
    VECTIS_HTTP_BIND_ADDR: "127.0.0.1:3100"
  workdir:
    copy: [init.json, init_pub.json, .unseal_key, vectis.sqlite3]
  service:
    ready: { url: "http://127.0.0.1:3100/healthz/ready", timeout_ms: 5000 }
```

- **`env_file`:** un archivo `.env` (relativo al YAML) con el entorno base. Una asignación `NOMBRE=valor` por línea, LF o CRLF, líneas vacías y comentarios `#` de línea completa. Se quitan espacios y tabuladores alrededor de nombres y valores. Un valor que empieza con comilla termina en la misma comilla y conserva su contenido. Se acepta un `export ` inicial. No hay escapes ni interpolación: `${NOMBRE}` queda literal. Un nombre repetido o una línea inválida dan error con su número. **Prioridad:** `env_file` < `env` < el valor que pone `envar` en el caso. Los nombres del `env_file` se pueden listar en [`hidden`](experiment.md#hidden-qué-se-oculta).
- **`workdir.copy`:** en cada caso se crea una carpeta temporal nueva, se copian ahí esos archivos (relativos al YAML) y el proceso arranca en ella. Al terminar el caso se borra. **No** se copia el `.env`: así un programa que también lee su `.env` solo ve lo que Nadir le pasa. Las rutas relativas del entorno apuntan a las copias, y los archivos que escriba el programa (bases de datos, logs) no ensucian la carpeta del experimento. Una entrada `{ from: origen, to: destino }` copia un archivo con otro nombre o en otra carpeta del directorio temporal, por ejemplo `{ from: vectis-dotenv.env, to: .env }` o `{ from: vectis.sqlite3, to: db/data.db }`; `to` tiene que ser una ruta relativa sin `..`.
- **`service.ready`:** en lugar de esperar a que el programa termine, Nadir sondea la URL (solo loopback) hasta que responde 2xx, el proceso termina o pasa `timeout_ms` (3000 por defecto). Si quedó listo, lo detiene con SIGTERM (SIGKILL a los 2 s) y guarda `process.stopped_code`. Si no quedó listo ni terminó a tiempo, lo mata y marca `process.timed_out`.
- **`print`** muestra el comando y cada regla de `envar` en una línea: `VAR={{values}}   (8 cases)` si muta una variable, o un `for` si muta varias, sin listarlas:

  ```
  ./vectis serve
  for VAR in 35 variables, one per case (123 cases):
    VAR={{spec}}
  VECTIS_HTTP_BIND_ADDR="127.0.0.1:3100"
  (+9 variables from ./.env)
    skipped: VECTIS_TLS_CERT_PATH, VECTIS_TLS_KEY_PATH (used when VECTIS_MODE=prod)
  ```

  Debajo van los valores de `env` (también los de variables mutadas: son la base que cambia cada caso; las referencias quedan sin expandir, `${env.NOMBRE}`) y cuántas variables vienen del `env_file`. Los valores de `hidden` salen como `[hidden]`. **`debug`** muestra solo la variable que cambió en ese caso, con el valor que tenía (`VECTIS_MODE="nadir-invalid"   (was: "dev")`), y el resultado (`result: exited 1 before ready · …` o `result: ready, stopped with 0`).

**Límite actual:** Los pasos CLI no generan `outputs` que otros pasos puedan consumir. Las referencias `${steps.<id>.outputs.<name>}` no funcionan entre pasos CLI. Esta limitación se levantará en una versión futura.

## `use: envar`: variar variables de entorno

El mutador `envar` cambia una variable de entorno del paso CLI. Solo se permite en pasos con `protocol: cli`. El nombre anterior `use: envvar` se sigue aceptando como alias. Cada regla debe elegir exactamente una de estas opciones:

```yaml
cases:
  mutators:
    # Forma de lista: todas las variables comparten el mismo mode (o path)
    - use: envar
      variables: [APP_LABEL, APP_PROFILE]
      mode: raw
      length: {reference: 64, max: 256}  # opcional

    # Forma de mapa: cada variable tiene su propia regla
    - use: envar
      variables:
        APP_OUTPUT_FORMAT:
          values: ["json", "text"]       # un caso por valor
        APP_LOCALE:
          value: "es-MX"                 # un caso con este valor fijo
        APP_PATH:
          path: ./payloads.txt           # un caso por línea del archivo
```

| Opción | Comportamiento |
|---|---|
| `mode: alpha` / `alphanum` / `num` / `raw` | Con `strategy: fuzz`, genera valores aleatorios: solo letras ASCII, letras y dígitos, solo dígitos o, con `raw`, **cualquier carácter salvo NUL** (puntuación, controles `0x01`-`0x1F` y DEL, saltos de línea, Unicode fuera de ASCII; el mismo conjunto que `raw` en `json`). Sin `length`, de 1 a 64 caracteres. La enumeración no produce casos. Solo en la forma de lista. |
| `mode: absent` | Un caso enumerado por variable, **sin** la variable: el proceso arranca como si nunca se hubiera definido (distinto de `VAR=`, que la define vacía). No admite `length`. Solo en la forma de lista; `debug` lo muestra como `VAR unset   (was: "…")`. |
| `mode: edge_cases` / `unconstrained` | Un caso enumerado por entrada del mismo catálogo que en `json` y `http`, con su nombre estable: `edge_cases` da `empty`, `long_lowercase`, `long_uppercase`, `unicode` y `reserved`; `unconstrained` da `empty`, `long_lowercase`, `huge_uppercase` (65 536 caracteres), `script_tag`, `sql_quote`, `template_syntax`, `null_string` y, solo en `envar`, `non_unicode` (bytes que no son UTF-8, que JSON y las cabeceras no pueden llevar). Se omite `nul_byte`, que una variable de entorno no puede llevar. No admite `length`. |
| `length: {reference, max}` | Junto a `mode`: la misma política de longitud que en `json`, con longitudes entre 0 y `reference - 1` o entre `reference + 1` y `max` (256 por defecto, hasta 4096). `reference` es obligatorio, porque una variable de entorno no tiene esquema del que leer un rango. Las longitudes cuentan caracteres, no bytes. |
| `value: "<cadena>"` | Un caso enumerado con ese valor fijo. Los números se tratan como texto para conservar ceros a la izquierda (`"001"`). |
| `values: ["a", "b"]` | Un caso enumerado por cada valor de la lista, en orden. En la forma de lista, junto a `variables: [A, B]`, vale para todas las variables (un caso por variable y valor). |
| `path: ./archivo.txt` | Un caso enumerado por línea del archivo (relativo al YAML), en orden. Admite cualquier texto UTF-8 salvo NUL (`../`, `;`, `$(…)`, comillas…). Las líneas en blanco se envían como valor vacío; se aceptan finales `\r\n`. Hasta 1 MiB y 4096 caracteres por línea. En la forma de lista, reemplaza a `mode` y vale para todas las variables. |

**`variables: all` y `variables: spec`:** en lugar de la lista de nombres, con las mismas opciones que la forma de lista (`mode`, `length`, `path` o `values`):

```yaml
- use: envar
  variables: [VECTIS_MODE]          # regla propia
  values: ["dev", "prod"]
- use: envar
  variables: all                     # todas las demás del entorno base
  mode: raw
  length: {reference: 16, max: 128}
- use: envar
  variables: spec                    # las de la especificación (con su used_when)
  spec: ./vectis-env.spec.yaml
  # only: [VECTIS_LOG_FILE, VECTIS_AUDIT_LOG_FILE]
  mode: unconstrained
```

- `all` son las variables del entorno base (`env_file` y `env`); `spec`, las de la especificación que su `used_when` permite (las omitidas salen en `print` como `skipped`), aunque el entorno base no las defina: se inyectan. `only` acota `spec`.
- Las dos toman solo las variables que **ningún otro** mutador `envar` del paso varía, así que una regla explícita gana siempre. Si no queda ninguna, es un error.
- A diferencia de `mode: spec`, que exige negarse a arrancar, estos casos los juzgan tus oráculos.

**Replay:** los casos de `value`, `values`, `path`, `absent`, `edge_cases`, `unconstrained` y `spec` se guardan con su posición o su nombre (`line: 3` en un archivo, `catalog: script_tag`) y se reconstruyen desde el YAML. Un caso de `path` guarda además el SHA-256 del archivo: si el archivo cambió, `replay` se niega (`the file of APP_PATH in step '…' changed since line 3 was saved`). Los casos aleatorios se repiten desde la semilla.

**Validación:**
- La variable debe existir en el entorno base (`env_file` o `env`); las de `mode: spec` se inyectan.
- Una misma variable no puede aparecer dos veces en el mismo mutador ni en varios mutadores del paso (dos reglas chocarían en `replay`). Usa otro paso con el mismo comando para combinar modos.
- La forma de lista requiere exactamente uno de `mode`, `path` o `values`. La de mapa requiere exactamente uno de `value`, `values` o `path` en cada variable, y no admite `mode`, `length`, `path` ni `values` en el mutador.
- `length` requiere `reference` y un modo que genere texto (`alpha`, `alphanum`, `num` o `raw`).
- Los campos de otros mutadores (`fields`, `seed`, `repetitions`, `keys`, `rename_to`, `expect_status`) y `only` fuera de `mode: spec` dan error en lugar de ignorarse.
- `print` muestra cada regla con su detalle: `APP_LABEL={{raw}}   (random values, length ~64, max 256)`, `APP_PATH={{file}}   (12 cases, from ./payloads.txt)`.

## `mode: spec`: valores que violan la especificación

Si el programa documenta sus variables, `mode: spec` las prueba una por una con valores que **no** cumplen esa documentación, y exige que el programa se niegue a arrancar.

```yaml
- use: envar
  mode: spec
  spec: ./vectis-env.spec.yaml
  # only: [VECTIS_MODE, VECTIS_SQLITE_PATH]
```

El archivo de especificación (relativo al YAML) se escribe una vez a partir de la documentación, `VARIABLE: { <tipo>: <opciones> }`:

```yaml
VECTIS_MODE:          { enum: [dev, prod] }
VECTIS_TLS_CERT_PATH: { file: { readable: true }, used_when: { VECTIS_MODE: prod } }
VECTIS_LOG_LEVEL:     { enum: [info, warn, error], skip: [uppercase, padded] }
VECTIS_CRYPTO_QUEUE_TIMEOUT_MS: { int: { min: 1, max: 30000 } }
APP_DATABASE_URL:     { dsn: { schemes: [postgres] }, required: true }   # sin valor por defecto
```

| Tipo | Casos (nombres estables) |
|---|---|
| `enum: [valores]` | `empty`, `other_word` (`nadir-invalid`), `uppercase`, `padded` (`" dev "`) |
| `bool` | `empty`, `uppercase` (`TRUE`), `numeric` (`1`), `word` (`yes`) |
| `int: {min, max}` | `empty`, `below_min`, `above_max`, `negative`, `fraction`, `not_a_number`, `overflow` |
| `file: {readable, writable, max_mode, max_size}` | `empty`, `missing`, `directory`, `unreadable`; con `max_mode: "0600"`, un caso por cada permiso de lectura o escritura del grupo u otros que exceda ese modo (`group_readable`, `other_readable`, `group_writable`, `other_writable`); con `max_size: 1024`, `too_large` |
| `dir` | `empty`, `missing`, `regular_file` |
| `filename` | `empty`, `with_separator` (`nadir/file`), `dot_dot`, `absolute` (`<temp>/nadir-absolute-file`: una ruta absoluta reemplaza el directorio base si el programa la une sin validar) |
| `socket_addr` / `host_port` | `empty`, `no_port`, `port_out_of_range`, `not_an_address` |
| `url: {schemes, query, fragment}` | `empty`, `no_scheme`, `wrong_scheme`, `with_query`, `with_fragment` |
| `http_path` | `empty`, `no_leading_slash`, `with_space` |
| `hex: {length}` | `empty`, `short`, `long`, `not_hex` |
| `domain` | `empty`, `with_space`, `too_long` |
| `dsn: {schemes}` | `empty`, `wrong_scheme`, `not_a_url` |
| `nonempty` | `empty` |

- **`non_unicode`:** todos los tipos incluyen además este caso: `nadir-nonutf8` seguido de los bytes `\xff\xfe`, que no son UTF-8. En Linux una variable de entorno puede llevar cualquier byte salvo NUL, y Nadir se lo pasa así al proceso; `debug` lo muestra como `nadir-nonutf8\xff\xfe` y la comprobación de eco busca `nadir-nonutf8`. Se omite con `skip: [non_unicode]`.
- **`used_when`:** la variable solo se prueba si la configuración base cumple la condición. Si no, `print` la lista como `skipped`.
- **`skip`:** casos que la documentación permite. Por ejemplo, `empty` cuando un valor vacío significa "usar el valor por defecto". También acepta `unset`.
- **`unset`:** cada variable que el entorno base define tiene además un caso **sin** la variable (si el entorno base no la define, quitarla es el caso base y no se repite). Se juzga según la regla:
  - con `required: true` (la variable no tiene valor por defecto), el programa debe negarse a arrancar: `startup:refuses`;
  - sin `required`, debe arrancar con su valor por defecto: `startup:defaults` (llegar a `ready`, o salir con 0 sin `service`). Si sale con error, lo más probable es que el valor por defecto no funcione o que la documentación no lo describa bien.
- **Las variables de la especificación** no tienen que estar en el entorno base: se inyectan.
- **`unreadable` y `regular_file`** apuntan a archivos que Nadir crea en la carpeta de `workdir`.
- **Permisos y tamaño:** los casos de `max_mode` y `max_size` parten del archivo de la propia variable (su valor en el entorno base, dentro de la carpeta de `workdir`): Nadir lo copia a `nadir-files/<VARIABLE>/<caso>` con el modo `max_mode` más un bit prohibido (`0600` → `0640` para `group_readable`), o relleno con saltos de línea hasta un byte más que `max_size`, así que lo único inválido es el permiso o el tamaño. Solo se generan si el entorno base define la variable.

**`startup:refuses`** es la regla implícita de cada caso de `mode: spec`. Tus oráculos del paso se evalúan solo en el caso base:

| Comprobación | Si falla |
|---|---|
| no llegó a `ready` y terminó con un código distinto de 0 | `AcceptedInvalidConfig`: arrancó (o salió con 0) con un valor prohibido; hay que investigarlo |
| no es un *panic* (101) ni una señal | `Crash` |
| no se quedó colgado | `Unresponsive` |
| la salida no contiene el valor inyectado (solo se buscan los que llevan `nadir`) | `Security`: el valor se filtra |
| la salida nombra la variable | `Assertion`: diagnóstico pobre |

**`startup:defaults`** juzga el caso `unset` de una variable opcional: falla con `Assertion` si el programa sale con error (con su última línea de stderr), con `Crash` si es un *panic* o una señal, y con `Unresponsive` si se queda colgado.

Un crash aparece como `startup:refuses · serve · VECTIS_MODE=other_word · started (ready)` o `startup:defaults · serve · VECTIS_SQLITE_PATH unset · exited 1 without VECTIS_SQLITE_PATH, which has a default: …`. Los casos de `value`, `values`, `path`, `absent`, `edge_cases`, `unconstrained` y `spec` (incluido `unset`) se guardan con su variable y su caso, y se repiten con `replay`.

**Presupuesto:** Los casos de `value`, `values`, `path`, `absent`, `edge_cases`, `unconstrained` y `spec` cuentan contra `max_cases`. Los modos generativos (`mode`) cuentan contra `max_execs` y usan la semilla de `fuzz`, igual que los demás mutadores.

## `use: envfile`: romper el archivo `.env`

Si el programa lee un archivo de configuración `NOMBRE=valor` de su directorio (un `.env`), `use: envfile` lo rompe de una forma distinta en cada caso y exige que el programa se niegue a arrancar. El archivo tiene que llegar por `workdir.copy`: el caso parte de esa copia válida, y el original junto al YAML nunca se toca.

```yaml
workdir:
  copy:
    - { from: vectis-dotenv.env, to: .env }
cases:
  mutators:
    - use: envfile
      file: .env                          # el destino de workdir.copy
      limits: { file_bytes: 65536, value_bytes: 4096, name_bytes: 128 }   # opcional
      prohibited: [VECTIS_UNSEAL_KEY]     # opcional
      # only: [duplicate_same, nul_byte]
```

| Caso | Qué cambia | Dónde debe apuntar el error |
|---|---|---|
| `duplicate_same` / `duplicate_other` | Agrega otra vez la primera asignación, igual o con otro valor | La línea nueva |
| `unknown_name` | Agrega `<PREFIJO>NADIR_UNKNOWN=…`, con el prefijo de la primera variable (`VECTIS_`) | La línea nueva |
| `foreign_name` / `lowercase_name` | Agrega `NADIR_FOREIGN=…` / la primera variable en minúsculas | La línea nueva |
| `no_equals` | Agrega una línea sin `=` | La línea nueva |
| `export_prefix` | Antepone `export ` a la primera asignación | Esa línea |
| `nul_byte` / `lone_cr` | Un NUL / un `\r` aislado en el valor de la última asignación | Esa línea |
| `unclosed_quote` / `text_after_quote` | `"valor` sin cerrar / `"valor" texto` en la última asignación | Esa línea |
| `invalid_utf8` | Bytes `\xff\xfe` en el último valor | El archivo |
| `unreadable` / `directory` | El archivo sin permisos / un directorio en su lugar | El archivo |
| `prohibited:NOMBRE` | Agrega cada variable de `prohibited` | La línea nueva |
| `value_too_long` / `name_too_long` / `file_too_large` | Un byte más que cada límite de `limits` (solo si se declara) | La línea, o el archivo para el tamaño |

**`envfile:refuses`** es la regla implícita de cada caso (los oráculos del paso solo juzgan el caso base). Falla con:
- `AcceptedInvalidConfig` si el programa arranca o sale con 0;
- `Crash` si es un *panic* (101) o una señal, y `Unresponsive` si se queda colgado;
- `Security` si la salida contiene el valor inyectado (cada uno lleva una marca `nadir-…` única);
- `Assertion` si el error no dice dónde: `line N` para los casos de línea, o el nombre del archivo para los del archivo entero.

Un crash aparece como `envfile:refuses · serve · unknown_name · exited 0`. Los casos se guardan por nombre (versión de registro 7) y `replay` reconstruye el archivo desde la copia.
