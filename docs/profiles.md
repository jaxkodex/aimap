# Profiles and patterns

A profile is a JSON object that Jev reads as `recipient_profile`. Jev keeps no
memory between requests, so this is how it learns what matters to you. Any
shape works. This one does well:

```json
{
  "who": "Alex, a freelance designer in Lisbon.",
  "active_priorities": ["Getting paid by clients", "Renewing the studio lease"],
  "low_value": ["Retail coupons", "Social media digests"]
}
```

Patterns are the kinds of mail you already know how to handle. When Jev
matches one with enough confidence, its labels are used as they are, which is
more consistent than judging each message from scratch. A CSV has one row per
pattern, or one row per example if you repeat the insight:

```csv
insight,importance,action_bucket,tags,example_from,example_subject
Client invoices,High,act_now,work;billing,Client <billing@client.example>,Invoice 42
Shop promotions,Low,discard,shopping;promo,Shop <deals@shop.example>,20% off this weekend
```

`importance` is `Low`, `Medium` or `High`. `action_bucket` is one of the seven
[action buckets](classification.md). JSON files take a list of objects with the same fields, plus
`examples` as a list of `{"from", "subject"}`.

```sh
aimap profiles set default --file profile.json   # create or replace
aimap patterns import default patterns.csv       # replaces the profile's patterns
aimap profiles list
aimap patterns list default
```

Every account uses `default` until you move it. For separate work and
personal labels:

```sh
aimap profiles set work --file work.json
aimap patterns import work work-patterns.csv
aimap accounts set-profile me@company.example work
```
