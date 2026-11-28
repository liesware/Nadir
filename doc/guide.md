# Guía: probar tu propio servicio

1. **Decide quién arranca tu API.** Puedes levantarla tú escuchando en `127.0.0.1` y un puerto (por ejemplo, 8080), o añadir un bloque [`target`](experiment.md#target-que-nadir-arranque-tu-servicio) para que Nadir la arranque y la pare. Con `target`, Nadir además detecta si tu programa se cae con alguna entrada.
2. **Ten un OpenAPI** con un `operationId` en cada operación que vayas a usar y, de preferencia, un `example` en cada `requestBody`. Así no tendrás que hacer `bind` de todos los campos obligatorios. Declara `bearerAuth` en `components.securitySchemes` y en `security` de las operaciones protegidas.
3. **Escribe el flujo feliz primero**, sin `cases`. Pon `setup: true` en las reglas imprescindibles (login, creación) y comprueba que el caso base pasa:
   ```bash
   nadir validate mi-experimento.yaml
   ```
   ```bash
   nadir run mi-experimento.yaml
   ```
4. **Añade oráculos que expresen invariantes** de tu negocio: "lo que se acepta (201) debe poder leerse", "lo que se rechaza (4xx) no debe guardarse", "un usuario sin permisos recibe 403", "la respuesta cumple el esquema" (`openapi_schema`).
5. **Añade `cases.mutators`** en los campos de entrada que quieras atacar, y `cases.examples` para valores concretos con un status esperado.
6. **Ajusta `max_cases`** para que quepan todas las variantes: 1 + número de variantes + número de ejemplos, hasta un máximo de 1000.
7. Cuando aparezca un hallazgo, **corrige el código y usa `nadir replay`** con el JSON del caso para confirmar el arreglo.

Consejos:

- Usa `${case.id}` en los campos que deban ser únicos (referencias, emails) para que los casos no choquen entre sí.
- Escribe las contraseñas como `${env.VAR}` y no directamente en el YAML.
- Un resultado `inconclusive` no es un hallazgo, pero hace que el código de salida sea `2`. Revisa timeouts (`timeout_ms`) y caídas del servicio.

## Ejemplos incluidos

| Archivo | Qué muestra |
|---|---|
Los tres funcionan con `./target/debug/nadir run examples/<nombre>/experiment.yaml`, sin variables de entorno: cada uno arranca y para su propia API de ejemplo.

| Ejemplo | Puerto | Qué muestra | Para ver hallazgos |
|---|---|---|---|
| [create-record](../examples/create-record/experiment.yaml) | 8080 | Flujo completo: login, token, mutadores `headers` y `json`, `when`, `record_absent`. 7 variantes. | `NADIR_FIXTURE_MODE=title_regression` → 1 hallazgo de regla; `error_on_reserved` → 1 hallazgo `ServerError` (500 con el título `RESERVED`); `exit_on_reserved` → hallazgo `process_exit` (más uno de `accepted-create-persists`, porque en ese modo la API tampoco guarda registros) |
| [auth-matrix](../examples/auth-matrix/experiment.yaml) | 8091 | Matriz de permisos con dos usuarios (admin y usuario normal), solo con el caso base. Las contraseñas por defecto son las de la API de ejemplo; si defines `NADIR_ADMIN_PASSWORD` y `NADIR_USER_PASSWORD`, la API y Nadir usan las tuyas. | `NADIR_FIXTURE_MODE=auth_matrix_open` → 2 hallazgos |
| [fuzz-probe](../examples/fuzz-probe/experiment.yaml) | 8081 | `strategy: fuzz` durante 5 segundos con `seed: 1`. Primero enumera 19 variantes: el paso `probe` envía 8 valores agresivos (`unconstrained`: byte NUL, cadena de 64 KiB, HTML, comilla SQL, `${}`…) y el paso `probe_schema` usa `mode: schema` más 3 valores propios. Después ejecuta unas 1200 iteraciones aleatorias. Ambos pasos validan la respuesta con `openapi_schema`. | `NADIR_FIXTURE_MODE=lenient` → la API deja de validar `/probe` y aparece 1 hallazgo `schema:rejects-invalid` con 7 casos aceptados |
