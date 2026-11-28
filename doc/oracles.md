# `oracles`: qué se considera un error

Cada paso tiene una lista de reglas que se evalúan en **todos** los casos (el base y cada variante).

| `use` | Qué comprueba | Falla cuando |
|---|---|---|
| `openapi` | Comprobación básica de conformidad | El status es **5xx** (categoría `ServerError`), aunque el OpenAPI lo documente; o el status **no está documentado** en las `responses` de la operación (una respuesta `default` documenta todos); o la respuesta dice ser JSON (`Content-Type`) pero no se puede parsear (categoría `Conformance`). **No** valida contra el esquema. |
| `openapi_schema` | Validación estricta de la respuesta | El cuerpo no cumple el JSON Schema del OpenAPI para ese status, o hay cuerpo cuando no debería haberlo. |
| `assert` | Una condición sobre un valor de la respuesta | La condición `expect` no se cumple. |
| `record_absent` | Que un recurso **no** exista (útil en seguridad) | El status es 200 y `record_reference` tiene un valor no nulo. Con 404 pasa; con cualquier otro status es `inconclusive`. |

Cada regla necesita un `id` único dentro de su paso, porque ese `id` nombra el hallazgo. La única excepción es `openapi`, cuyo `id` por defecto es `openapi`. `nadir validate` rechaza una regla sin `id`, un `id` repetido y un `kind` distinto de `security` o `check`.

Campos comunes:

```yaml
oracles:
  - use: openapi                  # el id por defecto es "openapi"

  - id: login-succeeds            # obligatorio en todas las demás reglas y único por paso
    use: assert
    setup: true                   # si falla en el caso base, se detiene todo
    select: response.status
    expect:
      equals: 200

  - id: accepted-create-persists
    use: assert
    when:                         # solo se evalúa si se cumple la condición
      select: steps.post_record.response.status
      equals: 201
    select: response.body.reference
    expect:
      equals: "${case.id}"

  - id: rejected-create-does-not-persist
    use: record_absent
    kind: security                # solo "security" o "check"; otro valor es un error
    when:
      select: steps.post_record.response.status
      equals: 422
    record_reference: response.body.reference
```

Opciones de `expect` (usa solo una):

| Opción | Pasa si… |
|---|---|
| `equals: <valor>` | el valor es igual. `null` se compara literalmente, y `"${case.id}"` se sustituye. |
| `one_of: [a, b, c]` | el valor es uno de la lista. |
| `exists: true` | el campo existe (aunque sea `null`). |
| `absent: true` | el campo no existe. |

Si `select` apunta a una respuesta que no existe (timeout o conexión cortada), el veredicto es `inconclusive`: nunca pasa ni falla por falta de datos.

## Selectores

**Entrada** (en `bind`): `body.<a.b.c>`, `path.<nombre>`, `header.<Nombre>`.

**Respuesta** (en `select`, `from`, `when.select`, `record_reference`):

| Selector | Significado |
|---|---|
| `response.status` | Código HTTP del paso actual |
| `response.header.Content-Type` | Cabecera de respuesta |
| `response.body.items[0].id` | Ruta JSON: puntos para claves y `[n]` para índices |
| `steps.<id>.response.<...>` | Lo mismo, pero de un paso **anterior** (solo en oráculos) |
