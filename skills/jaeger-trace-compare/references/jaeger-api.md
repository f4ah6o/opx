# Jaeger API Notes

## Endpoint

- `GET /api/traces?service=<service>&limit=<n>`

## Relevant fields

- `traceID`
- `spans[]`
  - `operationName`
  - `duration` (microseconds)
  - `startTime`
  - `references`
  - `processID`
- `processes[processID].tags[]`
  - contains resource attributes (including `git.commit` and `service.version`)

## Root span detection

A root span has empty `references`.

## Child span detection

A direct child has a `references[]` item where:

- `refType == "CHILD_OF"`
- `spanID == <root spanID>`
