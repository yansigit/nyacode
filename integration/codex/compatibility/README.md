These fixtures preserve the public RPC schemas and durable storage inputs from
Nyacode `b8bbac5e2ad84168eee5a8b08564a910e8006564`. Schema generation writes to
the assembled engine; it must never update these fixtures automatically.

Run `python3 scripts/engine_compatibility.py --workspace .engine/workspace` after
the engine's schema fixture checks. New optional properties and definitions are
allowed. Enum or exclusive-union changes, removed methods, changed fields or descriptions,
new required fields, and edits to existing migrations or provider storage
fixtures require a compatibility review. New migrations are allowed. Semantic
behavior is also checked by the provider, Chat recovery, and SDK suites.
