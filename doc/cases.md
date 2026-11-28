# `cases`: qué variar

Hay dos mecanismos, y se pueden usar juntos.

## `mutators`: catálogos de valores

**`json`**: varía campos del cuerpo JSON.

```yaml
cases:
  mutators:
    - use: json
      fields: [title, metadata.source]   # campos del body; los anidados con puntos
      mode: schema                       # edge_cases (por defecto) | unconstrained | schema | alpha | alphanum | raw
      values: ["../../etc/passwd", 0, {"a": [1]}]   # opcional: tus propios valores, de cualquier tipo
```

| `mode` | Valores que prueba | ¿Se puede reproducir con `replay`? |
|---|---|---|
| `schema` | Valores derivados del esquema de cada campo en el OpenAPI (ver la tabla siguiente). El campo tiene que existir en el esquema de la petición; si no, la validación falla. | Sí |
| `alphanumeric` | Con `strategy: fuzz`, genera cadenas ASCII alfanuméricas de hasta 256 caracteres. Incluye la cadena vacía, longitudes alrededor de la del valor base y longitudes aleatorias. La enumeración solo usa los `values` explícitos. | Sí |
| `alpha` / `alphanum` | Con `strategy: fuzz`, genera letras ASCII o letras y dígitos ASCII, respectivamente. Con una política de longitud, el generador elige con igual probabilidad una longitud menor o mayor que la del campo. `alphanum` equivale a `alphanumeric`. | Sí |
| `raw` | Con `strategy: fuzz`, genera cualquier carácter: la mitad ASCII imprimible (con signos de puntuación y espacio), una quinta parte controles (`\u0000`–`\u001F` y `\u007F`, incluidos tabulador y salto de línea) y el resto Unicode que suele romper validaciones: acentos, alfabetos de derecha a izquierda, CJK, emoji fuera del BMP, caracteres combinantes, espacios de ancho cero, `\u202E`, BOM, separadores de línea, no-caracteres (`\uFFFF`, `\u{10FFFF}`). Necesita una política de longitud, igual que `alpha`. | Sí |
| `num` | Con `strategy: fuzz`, genera solo dígitos ASCII, con la misma política de longitud que `alpha` y `raw` (`length` o el `minLength`/`maxLength` del esquema). En `json` viaja como texto (`"98765"`): los tipos numéricos (`1.5`, desbordamientos) los cubre `mode: schema`. | Sí, desde la semilla |
| `edge_cases` (por defecto) | `""` (vacío), `"a"×200`, `"A"×200`, `"测试"` (Unicode), `"RESERVED"` | Sí |
| `unconstrained` | `""`, `"a"×200`, `"\u0000"`, `"A"×65536`, `"</script>"`, `"' OR '1'='1"`, `"${}"`, `"null"` | Sí. Cada valor tiene un nombre estable en el caso guardado: `empty`, `long_lowercase`, `nul_byte`, `huge_uppercase`, `script_tag`, `sql_quote`, `template_syntax`, `null_string` |
| `schema_valid` | Alias antiguo de `edge_cases`. Muestra un aviso. | Sí |

Ningún modo comprueba antes de enviar si el valor cumple el esquema: los valores se envían tal cual, aunque violen `maxLength` u otras restricciones, y eso es intencional.

Una política de longitud se aplica a un solo campo. `length: {reference: 64, max: 256}` genera longitudes entre 0 y 63 o entre 65 y 256. Sin `reference`, Nadir usa `minLength` y `maxLength` si el esquema declara ambos. El máximo de generación es 256 por defecto y se puede configurar hasta 4096. `alpha` y `raw` requieren una referencia o un rango del esquema. Las longitudes cuentan caracteres, no bytes: con `raw`, 64 caracteres pueden ocupar hasta 256 bytes en UTF-8. Los valores siempre son texto Unicode válido, porque viajan dentro de una cadena JSON. Para probar bytes UTF-8 inválidos o JSON mal formado, usa las pruebas de sintaxis. `alphanum` sin bloque `length` conserva la distribución de `alphanumeric`.

**`values`** añade tus propios valores después del catálogo del modo, en el orden en que los escribes. Pueden ser texto, números, `null`, listas u objetos. En el caso guardado se identifican por su posición (`{"kind": "custom", "option": 0}`).

**Casos de `mode: schema`.** Se generan según lo que declare el esquema del campo:

| Si el esquema declara… | Casos (nombre en el caso guardado → valor enviado) |
|---|---|
| el campo en `required` | `missing_field` → el campo se quita del cuerpo |
| un `type` | `null_value` → `null` (salvo que admita `null`) y un valor de cada tipo que no admite: `wrong_type_string` → `"nadir"`, `wrong_type_number` → `12345`, `wrong_type_boolean` → `true`, `wrong_type_array` → `[]`, `wrong_type_object` → `{}` |
| `minLength` ≥ 1 | `too_short` → `minLength - 1` caracteres |
| `maxLength` | `at_max_length` → justo `maxLength` caracteres (es válido; sirve para detectar errores de "uno de más" en la base de datos) y `over_max_length` → `maxLength + 1` |
| `format` | `bad_format` → `"not-a-valid-<format>"` |
| `enum` | `not_in_enum` → `"nadir-not-in-enum"` (o `987654321` si el `enum` es numérico) |
| `minimum` / `exclusiveMinimum` | `below_minimum` → `minimum - 1`, o el propio límite exclusivo. Sin límite inferior: `negative_one` → `-1` |
| `maximum` / `exclusiveMaximum` | `above_maximum` → `maximum + 1`, o el propio límite exclusivo |
| `type: integer` | `fraction` → `1.5`, `integer_overflow` → `18446744073709551615` |
| `type: number` | `huge_number` → el mayor `f64` |
| `minItems` ≥ 1 / `maxItems` | `empty_array` → `[]` / `over_max_items` → `maxItems + 1` elementos `null` |
| un objeto con `required` | `empty_object` → `{}` |

El esquema se lee siguiendo `properties` (las `$ref` ya vienen resueltas). `allOf`, `oneOf` y `anyOf` no se recorren.

**Regla implícita `schema:rejects-invalid`.** Todos los casos de `mode: schema` salvo `at_max_length` envían un valor que el esquema prohíbe, así que el paso debe **rechazarlo** con un status ≥ 400. Si responde 2xx o 3xx, la regla falla. Funciona igual que `example:<id>`: no hace falta escribirla. Como los hallazgos se agrupan por regla, la salida muestra el primer caso y lista los demás que también fallaron:

```text
step probe_schema, rule schema:rejects-invalid: fail; expected rejection (status >= 400), observed 200
  same rule also failed in: 0010.json (null_value), 0011.json (wrong_type_number), …, 0016.json (over_max_length)
```

Los valores de `values` no llevan esta regla, porque Nadir no sabe si son válidos. Para ellos, escribe tus propias reglas.

## `http`: cabeceras y parámetros de ruta

`use: http` muta cabeceras y parámetros de ruta igual que `use: json` muta el cuerpo. Los campos usan la misma sintaxis que `bind`:

```yaml
cases:
  mutators:
    - use: http
      fields: [header.X-API-Key]          # puede tener bind: el caso base usa la key válida
      mode: raw
      length: {reference: 64, max: 256}
    - use: http
      fields: [path.kid, header.X-Request-Id]
      mode: unconstrained
      values: ["../../etc/passwd"]
    - use: http
      fields: [header.X-API-Key]
      mode: absent                        # el caso se envía sin la cabecera
```

**Modos:** los mismos que en `json` (`edge_cases` por defecto, `unconstrained`, `schema`, `alpha`, `alphanum`, `num` y `raw` con `length`), más dos propios:
- **`values`:** sin catálogo; solo tus `values`.
- **`absent`:** un caso sin la cabecera (o con el segmento de ruta vacío). Sirve, por ejemplo, para comprobar que sin `X-API-Key` la API responde 401.

**Con `bind`:** si el campo tiene `bind`, el caso base envía ese valor y cada caso mutado lo **reemplaza**; la cabecera nunca sale duplicada. Así se prueba "key válida en el caso base, keys alteradas en los casos mutados".

| | `header.*` | `path.*` |
|---|---|---|
| Qué se envía | El valor tal cual. Una cabecera admite ASCII visible, espacio, tabulador y caracteres no ASCII (viajan como bytes UTF-8); no admite caracteres de control ni saltos de línea | El valor codificado como **un segmento**: `../a b` sale como `..%2Fa%20b`. Solo `A-Z a-z 0-9 - . _ ~` van sin codificar |
| `raw` | ASCII imprimible, tabulador y Unicode, sin caracteres de control | Todo el alfabeto `raw`; se codifica al enviar |
| `unconstrained` | Sin `nul_byte`, que no se puede enviar | Catálogo completo (`\u0000` sale como `%00`) |
| `absent` | Quita la cabecera, aunque tenga `bind` | Segmento vacío (`/sign/`) |
| `schema` | Casos del esquema del parámetro en el OpenAPI (`type`, `enum`, `minLength`/`maxLength`, `format`, `minimum`…). Los valores que no son texto se envían como texto (`12345`, `null`). Si el parámetro es obligatorio, incluye `missing_field` (sin la cabecera). El campo tiene que estar declarado como parámetro: `X-API-Key` suele ser un esquema de seguridad (`apiKey`), y ahí `schema` da error | Igual |

- `length` necesita exactamente un campo. Sin `reference`, usa `minLength` y `maxLength` del esquema del parámetro.
- `values` solo admite texto.
- `print` muestra cada parte con sus modos: `X-API-Key: {{raw|absent}}`, `GET /items/{{unconstrained}}`.
- **Límites:** no se pueden enviar `\r`, `\n` ni otros caracteres de control en una cabecera (el cliente HTTP los rechaza), así que la inyección de cabeceras (CRLF) no se puede probar. Los parámetros de query (`?a=b`) todavía no se admiten.

**`headers`** es el nombre anterior y sigue funcionando: `use: headers` con `fields: [Accept-Language]` y `values: ["en", "es"]` equivale a `use: http` con `fields: [header.Accept-Language]` y `mode: values`. No admite `mode` ni `length`. Los casos guardados con `headers` se siguen repitiendo con `replay`.

Cada combinación de campo y valor genera **una variante**. Con `fields: [title]` en modo `edge_cases` y una cabecera con 2 valores obtienes 7 variantes.

## `path`: un caso por línea de un archivo

`use: json` y `use: http` aceptan `path` en lugar de `mode`: un caso enumerado por cada línea del archivo (relativo al YAML) y por cada campo de `fields`, en orden.

```yaml
- use: http
  fields: [header.X-API-Key, path.kid]
  path: ./payloads.txt
- use: json
  fields: [message_hash.hex, message_hash.alg]
  path: ./payloads.txt
```

- **Contenido:** cualquier texto UTF-8, hasta 1 MiB y 4096 caracteres por línea. Las líneas en blanco se envían como valor vacío; se aceptan finales `\r\n`.
- **Según dónde viaja:** en `json` todo se envía, incluidos NUL y controles (van escapados en el JSON); en una ruta, todo se codifica como un segmento (`../etc` → `..%2Fetc`); en una cabecera se omiten las líneas con caracteres de control, que el cliente HTTP no puede enviar.
- **Con `values`:** se envían después de las líneas. `path` no admite `mode` ni `length`.
- **`print`:** `X-API-Key: {{file}}` y la nota `header.X-API-Key, path.kid: file, 12 lines from ./payloads.txt`.
- **Replay:** cada caso guarda su línea y la huella del archivo. Si el archivo cambió, `replay` se niega: `the file of path.kid in step '…' changed since line 3 was saved`.

`path` es la forma general. `use: file` y `use: file_fuzz` (abajo) se mantienen para los experimentos que ya los usan: recorren las líneas en ciclo hasta `repetitions` y solo admiten letras, dígitos y espacios en un campo del cuerpo.

## `use: body`: caracteres aleatorios en el cuerpo JSON

Los demás mutadores producen JSON válido (los caracteres raros van escapados dentro de una cadena). `use: body` rompe el **texto** del cuerpo: en cada caso aleatorio toma el cuerpo que el paso enviaría, ya serializado y con los `bind` aplicados, y le hace de 1 a `edits.max` cambios en posiciones al azar.

```yaml
cases:
  mutators:
    - use: body
      mode: raw            # alpha, alphanum, num o raw (por defecto raw)
      edits: { max: 4 }    # por defecto 4, hasta 64
```

- **Cambios:** insertar de 1 a 4 caracteres, borrar un tramo de hasta 8, reemplazar un carácter o duplicar un tramo de hasta 16.
- **Qué se inserta:** la mitad de las veces un carácter del modo, la otra mitad uno de estructura de JSON (`{ } [ ] : , " \`), que son los que rompen el parser. Con solo letras, casi todos los cambios caerían dentro de una cadena.
- **El mensaje HTTP sigue siendo válido:** el cliente recalcula `Content-Length` y mantiene `Content-Type: application/json`. Para romper el HTTP, usa [`http_raw`](http-raw.md).
- **Requisitos:** `strategy: fuzz` (son casos aleatorios: cuentan contra `max_execs` y se repiten desde la semilla) y una operación con cuerpo `application/json`.
- **`print`:** `body: random edits (raw, 1 to 4 per case: insert, delete, replace, duplicate)`. `debug` muestra el cuerpo ya roto.

**`body:handled`** es la regla implícita de estos casos: el servicio debe responder **sin 5xx** antes del timeout. Un 4xx es lo esperado, y un 2xx también se acepta, porque a veces el cambio cae dentro de una cadena y el JSON sigue siendo válido. A diferencia de `wire:handled`, cerrar la conexión sin responder falla (`Unresponsive`), porque el mensaje HTTP es válido. Tus oráculos del paso solo juzgan el caso base: un `expect: 200` fallaría casi siempre con un cuerpo roto.

## Corpus de texto por líneas

En `cases.mutators`, `use: file` envía cada línea tal como se leyó. `use: file_fuzz` usa cada línea como punto de partida, conserva el prefijo compatible con el alfabeto elegido y ajusta la longitud con una semilla fija.

```yaml
cases:
  mutators:
    - use: file_fuzz
      fields: [title]
      path: ./validation-corpus.txt
      repetitions: 100
      mode: alphanum
      seed: 7
      length: {reference: 64, max: 256}
```

El archivo UTF-8 se lee desde la carpeta del experimento. Admite letras, dígitos, espacios y tabulaciones. Se preservan las líneas en blanco como cadenas vacías. El archivo puede tener hasta 1 MiB y cada línea hasta 4096 caracteres. Con `file` se omiten `mode`, `seed` y `length`.

Al llegar al final, Nadir vuelve a la primera línea. Cada repetición consume un caso de `max_cases`, además del caso base. Los casos de archivo se ejecutan después de los ejemplos, antes de la fase aleatoria general. `replay` verifica la huella del archivo antes de enviar el caso.

## `examples`: casos concretos con status esperado

Sirven para probar un valor específico y exigir un código HTTP exacto:

```yaml
cases:
  examples:
    - id: reserved-title-rejected
      field: title              # campo del body
      value: "RESERVED"         # cualquier valor JSON
      expect_status: 422        # status exacto esperado en ESTE paso
```

## Nombres de claves y sintaxis JSON

`fields` selecciona valores y `keys` selecciona nombres de propiedades. Con `strategy: fuzz`, un mutador con `keys` y `mode: alpha`, `alphanum` o `raw` genera el nombre nuevo durante la ejecución. Cada caso de renombrado cambia una sola propiedad y conserva su valor. Los nombres generados tienen entre 4 y 24 caracteres (`length` no se aplica a los renombres) y evitan colisiones con las demás propiedades del objeto. `expect_status` añade una comprobación opcional por caso.

```yaml
cases:
  mutators:
    - use: json
      keys: [message_hash.alg, message_hash.hex]
      mode: alpha
      expect_status: 400
  examples:
    - id: invalid-json
      syntax: trailing_comma
      expect_status: 400
```

Las pruebas de sintaxis usan documentos fijos con `syntax: incomplete_object`, `trailing_comma`, `leading_text` o `trailing_text`. Requieren una operación con cuerpo JSON y un `expect_status` de la familia `4xx`. Las pruebas de sintaxis consumen el presupuesto de `max_cases` y se pueden reproducir con `replay`.

Los renombres generados consumen el presupuesto aleatorio de `max_execs` y se regeneran con la semilla y la iteración guardadas. Para regresiones explícitas, un mutador con `keys`, `rename_to` y `expect_status` sigue generando un caso enumerado por cada pareja de nombres.

Los ejemplos se ejecutan después de las variantes y también consumen `max_cases`. Cada uno añade una regla implícita llamada `example:<id>`, que falla si el status no coincide.
