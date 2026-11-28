# `http_raw` y `malform`: peticiones mal formadas

`protocol: http` prueba la **lógica de tu API** con peticiones HTTP bien formadas. `protocol: http_raw` prueba **la capa HTTP** (el parser del servidor, por ejemplo hyper/axum) con peticiones **mal formadas**: un `Content-Length` falso, líneas que terminan solo en `\n`, cabeceras rotas, peticiones cortadas… Se declara como un paso más, aunque use el mismo endpoint, y en cada caso los pasos se ejecutan una vez y en orden:

```yaml
steps:
  - id: sign
    protocol: http                     # capa de aplicación
    operation_path: "/sign/{kid}.post"
    # …
  - id: axum
    protocol: http_raw                 # capa HTTP, mismo endpoint
    operation_path: "/sign/{kid}.post"
    body_example: { message_hash: { alg: SHA-256, hex: "0c51…6167" } }
    bind:
      path.kid: fcb7…44da
      header.X-API-Key: "${env.NADIR_VECTIS_API_KEY}"
    cases:
      mutators:
        - use: malform
          mode: catalog                # por defecto
          # only: [short_length, length_and_chunked, bare_lf]
    oracles:
      - { id: base-signs, use: assert, select: response.status, expect: { equals: 200 } }
```

**Un paso `http_raw`:**
- Usa la configuración de `protocols.http` (`base_url`, `openapi`) y arma la petición igual que `http` (`operation_*`, `bind`, `body_example`, `auth`). La escribe byte a byte en un socket TCP de loopback, con `Host`, `Content-Length` y `Connection: close`. El caso base también va por este socket, así se comprueba que el endpoint responde antes de romperlo.
- Solo admite `use: malform`. Otro mutador, `cases.examples` u `outputs` dan error, y `malform` en un paso `http` también.
- Después de enviar, Nadir cierra su lado de escritura. Un servidor que espera bytes que la petición prometió (un `Content-Length` largo, una línea vacía que no llega) recibe así un fin claro y puede responder o cerrar.

**`mode: catalog`:** una lista fija de malformaciones con nombre. Cada una parte de la petición bien formada, cambia **una sola cosa** y genera un caso. `only` elige algunas; sin `only`, se ejecutan todas. El caso guardado lleva el nombre y `replay` reconstruye los mismos bytes. `mode: random` (corrupción aleatoria de bytes) llegará en una fase posterior.

| Zona | Malformaciones |
|---|---|
| Línea de petición | `bad_method` (`P0ST`), `lowercase_method` (`post`), `bad_version` (`HTTP/9.9`), `missing_version`, `double_space` (`POST  /ruta`), `huge_uri` (64 KiB en la query), `nul_in_path`, `cr_in_path` |
| Cabeceras | `crlf_injection` (un `\r` suelto dentro de un valor: `X-Nadir: nadir\rX-Injected: 1`), `bare_lf` (todas las líneas con `\n`), `header_without_colon`, `space_before_colon` (`Host : …`), `folded_header` (valor continuado en la línea siguiente), `many_headers` (10,000), `huge_header` (64 KiB), `missing_host`, `duplicate_host` |
| Cuerpo | `short_length` (`Content-Length` de la mitad), `long_length` (+100), `negative_length` (`-1`), `non_numeric_length`, `conflicting_lengths` (dos `Content-Length` distintos), `length_and_chunked` (`Content-Length` y `Transfer-Encoding: chunked` a la vez), `bad_chunk_size`, `unterminated_chunked`, `trailing_bytes` (bytes de más después del cuerpo) |
| Final | `no_header_terminator` (sin la línea vacía), `truncated_request` (cortada a la mitad de las cabeceras) |

Las malformaciones del cuerpo usan un cuerpo de relleno si la petición no tiene uno.

**`wire:handled`** es la regla implícita de cada caso mal formado. Tus oráculos del paso se evalúan solo en el caso base (en los casos mal formados quedan `skipped`):

| El servidor… | Veredicto |
|---|---|
| responde 4xx | correcto |
| cierra o reinicia la conexión sin responder | correcto (no marca el caso como incompleto) |
| responde 2xx/3xx | correcto, con el status registrado: HTTP/1.1 permite tolerar algunas formas (líneas solo con `\n`, bytes de más como una segunda petición) |
| responde 5xx | **hallazgo** `ServerError` |
| no responde ni cierra antes de `timeout_ms` | **hallazgo** `Unresponsive` |
| se cae | **hallazgo** `process_exit` |

En la pantalla, `print` muestra la petición base y `malform: catalog, 28 cases`. `debug` muestra los bytes exactos que se enviaron: una fila por cada `\r\n`, con un `\r` o `\n` suelto y los caracteres invisibles escapados. Un crash aparece como `wire:handled · axum · length_and_chunked · HTTP 500`.
