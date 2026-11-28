# Pantalla, resultados y reproducción

## `print` y `debug`: ver las peticiones

Dos campos opcionales de cada paso muestran qué envía Nadir, sin frenar la ejecución: los casos corren a la velocidad normal. Las dos salidas aparecen en la pantalla de `nadir run`, entre los contadores y `Stop reason`.

```yaml
steps:
  - id: sign
    print: true          # muestra una vez la plantilla de la petición
    debug: true          # muestra una petición con su respuesta; espacio pasa a la que esté en curso
    # debug: {auto: 10}  # o cambia sola cada 10 segundos
```

**`print: true`** muestra la petición en formato HTTP. Cada parte que se muta aparece como `{{modo}}` (`{{raw}}`, `{{alphanum}}`, `{{schema}}`, `{{headers}}`, `{{file}}`…). Las referencias aparecen con su nombre (`{{env.API_KEY}}`, `{{case.id}}`, `{{steps.login.outputs.token}}`), y el largo como `{{len}}`. Debajo hay una línea por mutador y por ejemplo que explica cada marcador:

```
── print: sign ──────────────────────────────────
POST /sign/fcb7280b…44da HTTP/1.1
Host: 127.0.0.1:3000
X-API-Key: {{env.NADIR_VECTIS_API_KEY}}
Content-Type: application/json
Content-Length: {{len}}

{"message_hash":{"{{raw}}":"SHA-256","{{raw}}":"{{raw}}"}}
  message_hash.hex: raw, length 0-63 or 65-256
  keys message_hash.alg, message_hash.hex: raw, one rename per case, expect 400
```

La plantilla junta todas las mutaciones, pero cada caso cambia solo una parte. Por ejemplo, un renombre cambia una sola clave.

**`debug`** muestra una petición real tal como se envió, con su `Content-Length` real y, debajo, **la respuesta a esa misma petición**. La pantalla se queda con esa petición mientras los casos siguen corriendo. Al presionar espacio (o cada N segundos con `auto`), cambia a la petición que esté en curso en ese momento:

```
── debug: sign · case 4512/10001 ────────────────
POST /sign/fcb7280b…44da HTTP/1.1
Host: 127.0.0.1:3000
X-API-Key: [hidden]
Content-Type: application/json
Content-Length: 87

{"message_hash":{"alg":"SHA-256","hex":"7f\u0000é<\u{202E}…"}}

response: 400 {"error":"message_hash.hex must contain only hexadecimal characters"}
[space] refresh · [q] quit          (con auto: refresh in 7 s · [space] now · [q] quit)
```

| Tecla | Qué hace |
|---|---|
| espacio | muestra la petición en curso y su respuesta (con `auto`, además reinicia la cuenta atrás) |
| `q`, `Esc` o Ctrl+C | termina: la petición siguiente no se envía y la ejecución acaba con el resumen normal y `Stop reason: StoppedByUser`. Si se presiona antes de enviar el caso base, no hay nada que guardar y sale con código `2` |

- **Secretos:** se muestran como `[hidden]` solo los valores declarados en [`hidden`](experiment.md#hidden-qué-se-oculta) y los outputs `secret: true`.
- **Caracteres invisibles:** los de control, los de ancho cero, los bidireccionales (`\u{202E}`), el BOM, los separadores de línea y los no-caracteres se muestran escapados como `\u{XXXX}`, pero se envían tal cual.
- **Cuerpos largos:** se cortan en pantalla e indican cuántos bytes faltan. Si la terminal es baja, se ocultan filas; la respuesta, las teclas y la lista de crashes siempre quedan visibles.
- **Sin terminal** (CI, `> archivo`): `print` se escribe una vez. `debug: true` escribe la primera petición con su respuesta; `debug: {auto: N}` escribe una cada N segundos.
- **Alcance:** solo se muestran peticiones de los pasos que tienen `debug`. `nadir replay` ignora los dos campos.
- `auto` va de 1 a 3600 segundos. `debug: false` equivale a no ponerlo.

## Resultados y reproducción

Cada `nadir run` crea una carpeta junto al YAML del experimento:

```text
.nadir/runs/<fecha>-<id>/
├── run.json          # resumen: huellas del experimento y del OpenAPI, caso base, hallazgos, motivo de parada
└── cases/
    ├── 0000.json     # caso base
    ├── 0001.json     # variante 1
    └── ...
```

Cada `cases/NNNN.json` contiene, por paso, el status, el resultado (`completed` o un error de transporte) y el veredicto de cada oráculo. Si una regla falla, también incluye `expected` y `observed`. Al final está el `regression_plan`, que identifica la variante (paso, selector y qué valor del catálogo) sin copiar el valor enviado:

```json
"regression_plan": {
  "experiment_sha256": "0cf7…",
  "openapi_fingerprints": { "openapi.yaml": "49c9…" },
  "variant": {
    "step_id": "get_template",
    "selector": "header.Accept-Language",
    "variant_id": { "kind": "header", "option": 1 }
  }
}
```

**`Duration:`** es el tiempo de la ejecución de los casos, desde que el servicio está listo hasta que termina. Mientras corre aparece debajo de `Findings:` en segundos enteros y se actualiza con cada caso (si una petición se queda esperando su timeout, el contador avanza al terminar esa petición). Al final queda con un decimal, por ejemplo `Duration: 55.3s`.

**Motivos de parada** (`Stop reason`): `EnumerationComplete` (todo ejecutado), `MaxCasesReached` (se acabó `max_cases`), `DeadlineExpired` y `MaxExecsReached` (con `fuzz`: se acabó `duration_s` o `max_execs`; ninguno de los dos cuenta como error), `SetupCheckFailed` (falló una regla `setup` en el caso base), `BaselineIncomplete` (el caso base no obtuvo respuesta en algún paso) `ProcessExit` (el servicio arrancado con `target` se cayó; queda un hallazgo con `"kind": "process_exit"`, `exit_code` y `signal` en `run.json`) y `StoppedByUser` (se presionó `q` en `debug`; no cuenta como error y lo ya ejecutado se guarda).

### `artifacts/crashes/`: cada crash en cuanto ocurre

Un **crash** es un caso que hace fallar un oráculo (un 500, un status no documentado, una regla tuya, `example:<id>`, `schema:rejects-invalid`…) o que tumba el servicio (`process_exit`). En el momento en que ocurre:

1. `Findings:` sube en la pantalla.
2. Nadir escribe el caso en `artifacts/crashes/DD-MM-YYYY-HHMMSS-xxxxxx.json` (hora local y 6 hex aleatorios), dentro de la **carpeta del YAML del experimento**, sin importar desde dónde se ejecute `nadir`.
3. Debajo de `print` y `debug` aparece una línea con la regla, el paso, el status y la ruta:

```
── crashes ─────────────────────────────────────
openapi · sign · HTTP 500 → ./artifacts/crashes/27-11-2026-184512-a3f9c1.json
process_exit · sign → ./artifacts/crashes/27-11-2026-184530-0b7e44.json
```

- **Formato:** el archivo es un registro de caso completo, igual que `cases/NNNN.json`, con los valores declarados en `hidden` y los outputs `secret: true` como `[hidden]`. Se repite con `nadir replay experimento.yaml --case artifacts/crashes/….json`.
- **Límites:** con `fuzz`, un mismo error puede repetirse miles de veces. Se escriben los mismos casos que se guardan en `.nadir/runs/` (hasta 5 por regla y 200 por ejecución en la fase aleatoria); las demás repeticiones solo suman en `Findings`.
- **Pantalla:** en vivo se ven los últimos 5 crashes; al final, hasta 20. Sin terminal, cada crash se escribe en una línea `crash: …` en cuanto ocurre.
- **Errores:** si el archivo no se puede escribir (permisos, disco lleno), la línea dice `crash file not written: …` y la ejecución sigue.
- **Git:** `.gitignore` ya ignora `**/artifacts/crashes/`.

### Diagnósticos fuera de los oráculos

Una salida confirmada del objetivo genera un YAML de solo lectura en `artifacts/crashes/`. Los errores de transporte se guardan en `artifacts/transport-errors/`. El YAML comparte nombre base y `case_id` con su registro JSON de diagnóstico. Incluye el paso, el método, la plantilla de ruta y los metadatos disponibles del cuerpo. Omite valores de encabezados, rutas resueltas y contenido del cuerpo.

Nadir guarda las salidas confirmadas como `process_exit`. Si el proceso sigue activo o no se puede confirmar su salida, el evento se registra como `transport_error`. Nadir guarda la primera captura por paso, motivo y fase de transporte por ejecución. La consola muestra estos eventos en `transport errors`. `Transport errors` cuenta todas las ocurrencias por separado de `Findings`. Un cierre antes de los encabezados que el propio caso acepta permanece en el registro del caso, fuera del contador y de las alertas de transporte. Una salida confirmada conserva su diagnóstico.

**`nadir replay`** vuelve a ejecutar la variante guardada contra el servicio *actual*:

- Antes de enviar nada (y antes de arrancar el `target`), comprueba que el caso guardado se puede repetir. Si no se puede, sale con `2` sin arrancar el servicio.
- Exige el **mismo contenido** del YAML del experimento, comprobado por su SHA-256.
- Si el OpenAPI cambió, avisa (`Replay used a changed OpenAPI contract`) pero continúa.
- Los registros de la versión 1 se pueden leer, aunque avisan de que el contrato original no se puede verificar.
- Es la forma de confirmar un arreglo: el código de salida pasa de `1` a `0`.

> Añade `.nadir/` a tu `.gitignore` si no quieres versionar la evidencia.
