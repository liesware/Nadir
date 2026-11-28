# Configuración: el archivo de experimento

## Estructura completa

```yaml
version: 1                       # obligatorio, solo existe la versión 1

experiment:
  name: mi-experimento           # nombre libre
  max_cases: 12                  # casos totales, incluido el caso base (1–1000)
  hidden: [API_KEY]              # opcional: variables cuyo valor sale como [hidden]
  exploration:
    strategy: enumerate          # opcional; "enumerate" (por defecto) o "fuzz", ver más abajo
    # max_fields: 2              # solo con fuzz: como mucho N partes por caso aleatorio

  target:                        # opcional: Nadir arranca y para tu servicio
    command: ["./mi-servidor", "--port", "${target.port}"]
    ready:
      path: /health

  protocols:
    http:
      base_url: http://127.0.0.1:8080   # solo IPs loopback
      openapi: ./openapi.yaml           # ruta relativa al YAML del experimento
      timeout_ms: 2000                  # opcional, por defecto 2000
      max_path_chars: 4096              # opcional: largo máximo de un parámetro de ruta aleatorio
    cli:
      timeout_ms: 5000                  # opcional, por defecto 5000

  steps:
    - id: login                  # identificador único del paso
      protocol: http             # http, http_raw (peticiones mal formadas) o cli (comando local)
      format: json               # opcional; JSON es el valor predeterminado
      operation_id: login        # operationId en el OpenAPI
      # operation_path: "/login.post"  # alternativa por plantilla y método
      bind: { ... }              # valores de entrada fijos
      auth: { ... }              # token bearer
      outputs: { ... }           # valores para pasos posteriores
      cases: { ... }             # qué variar en este paso
      oracles: [ ... ]           # qué comprobar en este paso
      print: true                # opcional: muestra la plantilla de la petición
      debug: true                # opcional: muestra cada petición antes de enviarla
      command: ["./app", "run"]  # protocol: cli — programa y argumentos
      env:                       # protocol: cli — entorno inicial
        VAR: valor
```

**`max_path_chars`** (solo `protocol: http`, de 1 a 65 534, por defecto 4096) limita el largo de los valores **aleatorios** de parámetros de ruta: las mutaciones que parten del catálogo o de tus `values` pueden crecer mucho, y codificadas con `%XX` armarían URLs más largas de lo que el cliente HTTP acepta (65 534 bytes). Con 4096 siempre caben; hasta 5461 también, porque un carácter ocupa como mucho 12 bytes codificado. Por encima, algunos casos pueden salir como `unsendable` (petición no enviable, no un fallo del servicio). Súbelo para apuntar al límite de URL del servicio (muchos responden `414` a partir de 8 KB). No afecta a los generadores (`alpha`, `raw`…, que usan su `length`), a los casos enumerados ni a `http_raw`, que escribe la petición byte a byte y no tiene ese límite.

`protocol` selecciona cómo se comunica Nadir con el objetivo y `format` cómo procesa el contenido. Los nombres anteriores `adapters` y `adapter` siguen siendo válidos como alias de `protocols` y `protocol`. En cada nivel se escribe un solo nombre.

Cada paso debe elegir un selector. `operation_path: "/sign/{kid}.post"` selecciona la operación `POST` de la plantilla exacta `/sign/{kid}`, aunque el OpenAPI omita `operationId`. El método admite mayúsculas y minúsculas. Los valores de parámetros como `kid` se siguen pasando con `bind`.

## `exploration`: enumeración o fuzzing

```yaml
exploration:
  strategy: fuzz      # "enumerate" (por defecto) | "fuzz"
  duration_s: 60      # obligatorio con fuzz: duración máxima de toda la ejecución, en segundos
  max_execs: 10000    # opcional (por defecto 10000): número máximo de casos aleatorios
  seed: 1             # opcional: semilla (distinta de 0); sin ella se elige una y se muestra
```

- **`enumerate`**: ejecuta el caso base, cada variante de los catálogos y los `cases.examples`, y termina.
- **`fuzz`**: hace lo mismo y después sigue con una **fase aleatoria** hasta que se agota `duration_s` (`Stop reason: DeadlineExpired`) o `max_execs` (`MaxExecsReached`). No hace falta el bloque `target`, pero con él Nadir también detecta si el servicio se cae.

**Cómo es un caso aleatorio.** Cada caso cambia partes de **un mismo paso** (una parte es un campo del cuerpo, una cabecera o un parámetro de ruta). Se arma en tres sorteos:

1. **Cuántas:** un número entre 1 y el total de partes distintas del paso (o `max_fields`, si lo pones), todos con la misma probabilidad.
2. **Cuáles:** esas partes, distintas y con la misma probabilidad cada una.
3. **Con qué mutador:** si una parte tiene varios mutadores (por ejemplo, dos `raw` con distinto `length` sobre `X-API-Key`), se sortea uno. La parte cuenta una sola vez.

Un caso de renombre de clave (`keys`) es aparte: cambia una sola clave y nada más. Con `N` partes, cada una entra en `(N+1)/(2N)` de los casos: con 3 partes, en 2 de cada 3. Si una parte bloquea a las demás (una key alterada da 401 antes de validar el resto), usa `max_fields: 1` o pon esa parte en un experimento aparte.

```yaml
exploration:
  strategy: fuzz
  duration_s: 60
  max_fields: 2      # opcional: como mucho 2 partes por caso (por defecto, todas)
```

En cuanto a los valores, el modo `alphanumeric` genera cadenas ASCII alfanuméricas con longitudes variables, y los modos `alpha`, `alphanum` y `raw` con `length` generan texto de su alfabeto con la longitud de su política. En los demás modos, cada valor sale de una de estas fuentes:
- un valor del catálogo del campo (incluidos los de `mode: schema` y tus `values`);
- una mutación de texto del valor base o de un valor del catálogo: insertar tokens peligrosos (`'`, `%s`, `{{7*7}}`, `../../`, `\0`, `\u202e`, números límite…), borrar o duplicar trozos, repetir el texto (hasta 100 000 caracteres), truncarlo o cambiar caracteres;
- un valor JSON extremo de cualquier tipo: enteros límite (`i64::MAX`, 2⁵³+1…), `1e308`, `5e-324`, cadenas largas, listas u objetos anidados.

En las cabeceras solo se envía ASCII visible, para que la petición se pueda construir.

**Reproducible.** Cada caso aleatorio depende solo de la semilla y de su número de iteración. Con la misma `seed` y el mismo experimento se repite la misma campaña (salvo dónde corte el tiempo). Un caso guardado lleva `"fuzz": {"seed": 7, "iteration": 67}`, y `nadir replay` lo regenera a partir de esos dos números. Los casos nuevos guardan además la regla con la que se generaron (`"generator": 2`) y, si lo usaste, `max_fields`: `replay` usa esos valores aunque después cambies el experimento. Los casos guardados antes de esta regla (sin `generator`) se regeneran con la regla anterior (de 1 a 3 campos), así que se repiten igual.

**Qué se guarda.** Solo los casos aleatorios que fallan alguna regla o tumban el servicio: como máximo 5 por regla y 200 por ejecución, para no llenar el disco. Los casos aleatorios sin respuesta utilizable (timeouts, conexiones cortadas) que no fallan ninguna regla solo se cuentan. `run.json` incluye un resumen en `"fuzz"` (`seed`, `executed`, `kept`, `incomplete`), y la salida muestra:

```text
Enumeration cases: 7
Random cases: 386 (kept 5, seed 7)
Findings: 1
Stop reason: DeadlineExpired
```

Si hubo casos aleatorios inconclusos y ningún hallazgo confirmado, la ejecución queda inconclusa y sale con código `2`.

Los casos aleatorios no llevan la regla implícita `schema:rejects-invalid`, porque Nadir no sabe si el valor mutado es válido. Encuentran errores a través de tus reglas, de `openapi` (5xx, status no documentados) y de las caídas del `target`.

## `max_cases`: el presupuesto

`max_cases` es el número máximo de ejecuciones del flujo, **contando el caso base**. Con `max_cases: 12` caben 1 caso base y 11 variantes o ejemplos. Con `strategy: fuzz`, `max_cases` solo limita la enumeración; la fase aleatoria tiene su propio presupuesto (`duration_s` y `max_execs`). Si se generan más casos enumerados que espacios disponibles, los que sobran no se ejecutan (en el orden de la enumeración, lo último es `malform` y `envar`; ver el [mapa de capacidades](capabilities.md#2-mutadores-por-protocolo)). No cuenta como error, pero Nadir lo avisa: el progreso muestra `Enumeration cases: 40/185 · max_cases runs 99` desde el principio, y el resumen dice `Enumeration cases: 99 of 185 (max_cases reached: 86 enumerated cases did not run; raise max_cases)`, aunque después siga la fase aleatoria y el motivo de parada final sea otro. El máximo permitido es 1000. Los casos se guardan como `cases/0000.json` … `cases/0999.json`.

## Referencias `${...}`

Se pueden usar en `bind`, `auth.bearer` y, en el caso de `${case.id}`, en `expect.equals`. En el bloque `target` solo se permiten `${env.…}` y `${target.port}`.

| Referencia | Valor |
|---|---|
| `${env.NOMBRE}` | Variable de entorno `NOMBRE`; si no está definida, la ejecución falla. Úsala para contraseñas reales y no las escribas en el YAML. |
| `${env.NOMBRE:-valor}` | Igual, pero usa `valor` si la variable no está definida o está vacía, como en bash. Útil para credenciales de prueba y para opciones como el modo del servidor. El valor no puede contener `}`. Si la variable está en `hidden` y se usa el valor por defecto, se oculta ese valor. |
| `${case.id}` | UUID único de cada caso. Sirve para crear recursos que no choquen entre casos. |
| `${steps.<id>.outputs.<nombre>}` | Salida extraída de un paso **anterior**. |
| `${target.port}` | Puerto tomado de `base_url`. También se puede usar en `target.command` y `target.env`. |

Puedes combinar texto y referencias: `"${case.id}-user"`.

## `target`: que Nadir arranque tu servicio

Este bloque es opcional. Si lo defines, `nadir run` y `nadir replay` arrancan tu servicio antes del caso base, esperan a que esté listo y lo paran al terminar, también cuando hay un error.

```yaml
target:
  command: ["python3", "-I", "../../tests/fixtures/python_api/server.py",
            "--port", "${target.port}", "--mode", "corrected"]
  ready:
    path: /health          # por defecto "/"
    timeout_ms: 10000      # por defecto 10000
  cwd: .                   # opcional; por defecto, la carpeta del YAML
  env:                     # opcional; se suman al entorno de Nadir
    LOG_LEVEL: debug
```

| Campo | Qué hace |
|---|---|
| `command` | Programa y argumentos. No pasa por una shell. `${target.port}` se sustituye por el puerto de `base_url`, y `${env.NOMBRE}` o `${env.NOMBRE:-valor}` por la variable de entorno. |
| `ready.path` | Nadir hace `GET` a esta ruta en la dirección de `base_url` hasta recibir un status 2xx. |
| `ready.timeout_ms` | Tiempo máximo de espera. Si se agota, Nadir para el proceso y sale con código `2`. |
| `cwd` | Directorio de trabajo, relativo al YAML, igual que `openapi`. Las rutas relativas de `command` (las que contienen `/`) también se resuelven desde ahí, así que el experimento funciona sin importar desde dónde ejecutes `nadir`. |
| `env` | Variables extra para el proceso. El proceso también hereda todas las variables de Nadir, como las contraseñas `${env.…}`. |

Cómo se comporta:

- **Puerto ocupado.** Si ya hay algo escuchando en la dirección de `base_url`, Nadir se niega a arrancar y sale con `2`. Así nunca prueba por error un servicio viejo.
- **El servicio no arranca.** Si el proceso termina o no responde a tiempo, el error incluye las últimas 20 líneas de su stderr. No se crea carpeta de resultados.
- **El servicio se cae durante un caso.** Se registra un hallazgo `process_exit` con el código de salida o la señal y el paso en el que ocurrió, la ejecución se detiene (`Stop reason: ProcessExit`) y se muestran las últimas líneas de su stderr. El caso se puede repetir con `nadir replay`, que arranca un servicio nuevo y sale con `1` si se vuelve a caer.
- **Al terminar.** Nadir envía SIGTERM y, si el proceso sigue vivo a los 2 segundos, SIGKILL.
- **Secretos.** En los extractos de stderr, el valor de cada variable de [`hidden`](#hidden-qué-se-oculta) se reemplaza por `[hidden]`; el resto de la línea se conserva.
- **Salida estándar.** stdout del proceso se descarta; solo se captura stderr (los últimos 64 KiB).

Para que la parada funcione, `command` debe arrancar directamente el proceso que escucha en el puerto. Si necesitas una shell, usa `exec`: `["sh", "-c", "exec ./servidor --port ${target.port}"]`. Si un lanzador como `npm` crea procesos hijos, puede que estos no reciban la señal.

> El bloque `coverage` dentro de `target` sigue sin estar disponible: si lo incluyes, `run` lo rechaza antes de arrancar nada.

## `bind`: entradas fijas

```yaml
bind:
  body.user: "admin"                                  # campo del cuerpo JSON
  body.metadata.source: nadir                         # campos anidados con puntos
  body.template_id: "${steps.get_template.outputs.template_id}"
  path.reference: "${case.id}"                        # parámetro de ruta /records/{reference}
  header.X-Request-Source: nadir                      # cabecera
```

- `body_example` permite declarar el cuerpo JSON inicial en el paso. Si se omite, Nadir usa el `example` del `requestBody` del OpenAPI cuando existe. Los valores de `bind` se aplican encima, y los campos que solo aparecen en el ejemplo se pueden mutar.
- Todos los parámetros de ruta obligatorios (`{id}`) deben tener un `bind` de tipo `path.`.
- Los valores pueden ser cadenas, números, booleanos u objetos JSON.
- **No se puede mutar un campo del cuerpo que tenga `bind`.** Nadir lo rechaza para que la variante sea inequívoca. Los campos se comparan por segmentos: `bind: body.metadata` choca con un mutador sobre `metadata.source`, pero `body.subtitle` no choca con `title`. Las cabeceras y los parámetros de ruta **sí** se pueden mutar aunque tengan `bind`: el caso base envía el valor del `bind` y cada caso mutado lo reemplaza (ver [`http`](cases.md#http-cabeceras-y-parámetros-de-ruta)).

## `auth`: autenticación

```yaml
auth:
  bearer: "${steps.login.outputs.token}"
```

Envía `Authorization: Bearer <token>`. La operación debe declarar `security: [bearerAuth: []]` en el OpenAPI. Si no la declara, la validación falla.

## `outputs`: pasar datos entre pasos

```yaml
outputs:
  token:
    from: response.body.access_token   # selector de respuesta
    when_status: 200                   # opcional: solo se extrae con este status (número o lista)
    secret: true                       # opcional: su valor sale como [hidden]
```

`secret: true` es la forma de ocultar un valor que no viene de una variable de entorno, como el token de un login. Su valor sale como `[hidden]` donde aparezca, y las respuestas de los pasos que lo usan en un `bind` no se copian a la evidencia.

## `hidden`: qué se oculta

Nadir solo oculta lo que declaras. `hidden` lista **variables de entorno**, y el valor de cada una se muestra como `[hidden]` donde aparezca:

```yaml
experiment:
  hidden: [NADIR_VECTIS_API_KEY]
```

- **Por valor:** se busca el valor real en la pantalla de `debug`, las respuestas, los archivos de caso y crashes, `run.json`, la salida de `replay` y el extracto de stderr del servicio. Una key que altera el fuzzer no es ese valor, así que se ve en claro: en `debug`, el caso base muestra `X-API-Key: [hidden]` y los casos mutados muestran la key enviada.
- **Valor por defecto:** si la variable no está definida y se usa `${env.X:-valor}`, se oculta `valor`.
- **Validación:** cada nombre tiene que usarse en algún `${env.NOMBRE}` (`bind`, `auth`, `target.command` o `target.env`) y no puede repetirse. Así una errata no deja un secreto visible.
- **Valores cortos:** un valor de menos de 4 caracteres da un aviso (`warning: hidden variable …`) y no se oculta, porque taparía texto por todas partes.
- **Sin `hidden` ni `secret: true`, no se oculta nada.** Tampoco `Authorization`: si el token viene de un login, márcalo con `secret: true`; si viene de una variable, añádela a `hidden`.
- **Un secreto escrito en el YAML** (`bind: header.X-Key: "abc123"`) no se puede ocultar. Pásalo a una variable: `"${env.X_KEY}"` y `hidden: [X_KEY]`.
- `print` nunca muestra valores reales: las referencias salen como `{{env.X}}`.
