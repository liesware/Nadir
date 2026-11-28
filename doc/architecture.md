# Arquitectura del código

```text
src/
├── cli/            argumentos (clap) y despacho de validate / run / replay
├── config/         tipos YAML crudos, validación estática, carga + huellas SHA-256
├── experiment/     resolución contra OpenAPI, selectores y referencias ${...}
├── adapters/http/  contrato OpenAPI, construcción de peticiones, transporte loopback
├── adapters/http_raw/ peticiones byte a byte por socket TCP, catálogo `malform`, lector de respuestas tolerante
├── execution/      WorkflowRunner: ejecuta los pasos, resuelve referencias, extrae outputs
├── mutation/       VariantGenerator: catálogos edge_cases / unconstrained / schema / http (cabeceras y ruta); fuzz.rs: casos aleatorios por semilla
├── exploration/    enumeración, fase aleatoria (fuzz.rs) y presupuesto (libafl/: base del futuro fuzzing con cobertura)
├── evaluation/     oráculos (openapi, openapi_schema, assert, record_absent)
├── finding/        agrupación (deduplicación) de hallazgos
├── storage/        escritura de .nadir/runs/<id>/
├── artifacts/      formato de los registros de caso y del plan de regresión
├── replay/         reconstrucción y re-ejecución de un caso guardado
├── reporting/      salida en la terminal
└── target/         arranque, espera de disponibilidad, vigilancia y parada del servicio (bloque target)
```

Flujo interno: `cli` → `config::load_resolved` (YAML + OpenAPI → `Experiment`) → `WorkflowRunner::from_resolved` → `target::launch` (si hay bloque `target`) → `exploration::run_enumeration` (caso base → `VariantGenerator::enumerate` → variantes → `EvaluatedCase`) → `storage` → `reporting`.

Ejecuta las pruebas con `cargo test`. Necesitan permiso para abrir sockets locales.
