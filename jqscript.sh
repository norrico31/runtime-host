jq -s '
  [.[] | select(
    .event_type == "request.timeout" or
    .event_type == "runtime.trap" or
    .event_type == "runtime.error" or
    .event_type == "request.failed" or
    .event_type == "instance.terminated"
  )]
  | .[]
  | {
      timestamp,
      event_type,
      request_id,
      instance_id,
      attributes
    }
' telemetry/events.jsonl
