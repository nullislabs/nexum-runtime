;; A component satisfying the event-module world surface (init and
;; on-event return `ok`) that also exports an interface instance, which
;; no in-tree guest can: world synthesis emits func exports only.
;; Byte-stable fixture; tests hash it at runtime for their pins.
;;
;; The named types are structural copies of nexum:host/types; runtime
;; type equality does not care where a type was declared. Each is
;; exported because an exported func may refer to named types only.
(component
  (core module $impl
    (memory (export "mem") 1)
    (global $next (mut i32) (i32.const 64))
    (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
      (local $ptr i32)
      (local.set $ptr
        (i32.and (i32.add (global.get $next) (i32.const 7)) (i32.const -8)))
      (global.set $next (i32.add (local.get $ptr) (local.get 3)))
      (local.get $ptr))
    ;; Each export returns a pointer to a zeroed result<_, fault>:
    ;; discriminant 0 is `ok`.
    (func (export "init") (param i32 i32) (result i32)
      (i32.const 8))
    (func (export "on-event") (param i32 i64 i64 i32 i32 i64) (result i32)
      (i32.const 8)))
  (core instance $i (instantiate $impl))
  (alias core export $i "mem" (core memory $mem))
  (alias core export $i "cabi_realloc" (core func $realloc))
  (alias core export $i "init" (core func $init-core))
  (alias core export $i "on-event" (core func $on-event-core))

  (type $rate-limit' (record (field "retry-after-ms" (option u64))))
  (export $rate-limit "rate-limit" (type $rate-limit'))
  (type $fault' (variant
    (case "unsupported" string)
    (case "unavailable" string)
    (case "denied" string)
    (case "rate-limited" $rate-limit)
    (case "timeout")
    (case "invalid-input" string)
    (case "internal" string)))
  (export $fault "fault" (type $fault'))
  (type $block' (record
    (field "chain-id" u64)
    (field "number" u64)
    (field "hash" (list u8))
    (field "timestamp" u64)))
  (export $block "block" (type $block'))
  (type $chain-log' (record
    (field "address" (list u8))
    (field "topics" (list (list u8)))
    (field "data" (list u8))
    (field "block-hash" (option (list u8)))
    (field "block-number" (option u64))
    (field "block-timestamp" (option u64))
    (field "transaction-hash" (option (list u8)))
    (field "transaction-index" (option u64))
    (field "log-index" (option u64))
    (field "removed" bool)))
  (export $chain-log "chain-log" (type $chain-log'))
  (type $chain-logs' (record
    (field "chain-id" u64)
    (field "logs" (list $chain-log))))
  (export $chain-logs "chain-logs" (type $chain-logs'))
  (type $tick' (record (field "fired-at" u64)))
  (export $tick "tick" (type $tick'))
  (type $custom-event' (record
    (field "kind" string)
    (field "payload" (list u8))))
  (export $custom-event "custom-event" (type $custom-event'))
  (type $event' (variant
    (case "block" $block)
    (case "chain-logs" $chain-logs)
    (case "tick" $tick)
    (case "custom" $custom-event)))
  (export $event "event" (type $event'))
  (type $config (list (tuple string string)))

  (func $init (param "config" $config) (result (result (error $fault)))
    (canon lift (core func $init-core) (memory $mem) (realloc $realloc)))
  (func $on-event (param "event" $event) (result (result (error $fault)))
    (canon lift (core func $on-event-core) (memory $mem) (realloc $realloc)))
  (export "init" (func $init))
  (export "on-event" (func $on-event))

  ;; The interface-instance export a provides claim is verified against.
  ;; It is empty because the check is nominal: the engine holds no WIT for
  ;; the interface to compare a surface against until #205.
  (instance $iface)
  (export "nexum:fixture/provider@1.2.3" (instance $iface))
)
