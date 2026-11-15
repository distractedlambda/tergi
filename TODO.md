# To fix

Known problems that nobody has fixed yet.

- **Extreme commit dates crash debug builds.** `format_date` and
  `format_day` add the time zone offset to the timestamp, and
  `short_date` subtracts it from the current time. Near `i64::MAX` or
  `i64::MIN` that overflows. Debug builds then panic and abort, for
  example on `format_date(i64::MAX - 100, 50400)`. Release builds wrap
  instead and show a nonsense date. Only a crafted commit has such a
  timestamp. Saturating arithmetic would fix it. See `src/util.rs`.
