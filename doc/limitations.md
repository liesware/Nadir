# Limitaciones

| Función | Estado |
|---|---|
| Fuzzing guiado por cobertura | No disponible: `strategy: fuzz` es de caja negra, sin información de qué código ejecuta cada caso. |
| Ctrl+C durante una ejecución larga | Detiene Nadir sin escribir `run.json`. Usa `duration_s` o `max_execs` para acotar la ejecución. |
| `target.coverage` (`mmap_file`, `afl_shm`) | Se valida, pero `run` lo rechaza. |
| Reinicio automático del `target` tras una caída | No disponible: una caída detiene la ejecución. |
| `nadir run --corpus` | No disponible. |
| Parámetros de *query string* | No hay selector de entrada para ellos. |
| Cuerpos que no son JSON | Solo se admiten cuerpos JSON. |
| Hosts que no son loopback | Se rechazan por diseño. |

El paquete Rust prohíbe el código `unsafe` (`unsafe_code = "forbid"`).
