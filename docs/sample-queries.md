# Sample queries

Generate the sample tree first, then run any query below against it:

```sh
cargo run --bin querymatter-samples -- --scale 1k samples
querymatter -e "SELECT count(*) AS total" samples
```

`samples/` is gitignored — regenerate it any time; generation is fully
deterministic (same build ⇒ byte-identical tree, mtimes included).

The whole file is also runnable in one shot via batch mode:

```sh
querymatter samples < docs/sample-queries.sql
```

Every result shown below assumes `--scale 1k`. `starwars/` output is
identical at every scale (that folder is fixed); the scaled folders
(`work/`, `recipes/`, `reading/`) change with `--scale`.

## The data

| Folder | Files at 1k | Theme |
| --- | --- | --- |
| `starwars/` | 35 (every scale) | The classic GraphQL star-wars cast: characters, starships, planets |
| `work/` | 483 | Work-doc hub: jira tickets with status, tags, nested estimates |
| `recipes/` | 289 | Recipe box: cuisines, timings, ingredient lists |
| `reading/` | 193 | Reading log by year: authors, ratings, series |

## Counting the tree

The simplest query — no `FROM`, so every discovered record is in scope.

```sql
SELECT count(*) AS total;
```

```
+-------+
| total |
+=======+
| 1000  |
+-------+
```

## Basic SELECT + WHERE with a numeric comparison

```sql
SELECT name, height_cm FROM 'starwars/characters/**' WHERE height_cm > 180 ORDER BY height_cm DESC;
```

```
+----------------+-----------+
| name           | height_cm |
+============================+
| Chewbacca      | 228       |
|----------------+-----------|
| Darth Vader    | 202       |
|----------------+-----------|
| IG-88          | 200       |
|----------------+-----------|
| Boba Fett      | 183       |
|----------------+-----------|
| Obi-Wan Kenobi | 182       |
+----------------+-----------+
```

## String equality WHERE

A quoted literal on the right of `=` forces a string comparison.

```sql
SELECT name, primary_function FROM 'starwars/characters/**' WHERE kind = 'droid' ORDER BY name;
```

```
+-------+------------------+
| name  | primary_function |
+==========================+
| C-3PO | Protocol         |
|-------+------------------|
| IG-88 | Assassin         |
|-------+------------------|
| R2-D2 | Astromech        |
+-------+------------------+
```

## SELECT * — every frontmatter key seen, sorted

Columns come out alphabetized, and a key some rows lack (`population` on
Dagobah) renders blank rather than dropping the column.

```sql
SELECT * FROM 'starwars/planets/**' ORDER BY name LIMIT 3;
```

```
+-----------+----------+------------+-------------------------+-----------------------+
| climate   | name     | population | residents               | terrain               |
+=====================================================================================+
| temperate | Alderaan | 2000000000 | Leia Organa             | grasslands, mountains |
|-----------+----------+------------+-------------------------+-----------------------|
| temperate | Bespin   | 6000000    | Lando Calrissian, Lobot | gas giant             |
|-----------+----------+------------+-------------------------+-----------------------|
| murky     | Dagobah  |            | Yoda                    | swamp, jungles        |
+-----------+----------+------------+-------------------------+-----------------------+
```

## DISTINCT drops duplicate projected rows

```sql
SELECT DISTINCT affiliation FROM 'starwars/characters/**' ORDER BY affiliation;
```

```
+----------------------+
| affiliation          |
+======================+
| Bounty Hunters Guild |
|----------------------|
| Cloud City           |
|----------------------|
| Galactic Empire      |
|----------------------|
| Hutt Cartel          |
|----------------------|
| Jedi Order           |
|----------------------|
| Rebel Alliance       |
+----------------------+
```

## file.* pseudo-columns

`file.*` columns come from the path and the file's stat, not frontmatter —
always available, even on records with sparse frontmatter.

```sql
SELECT file.name, file.folder, file.size, file.word_count FROM 'starwars/starships/**' ORDER BY file.size DESC LIMIT 5;
```

```
+----------------------------+--------------------+-----------+-----------------+
| file.name                  | file.folder        | file.size | file.word_count |
+===============================================================================+
| millennium-falcon.md       | starwars/starships | 320       | 13              |
|----------------------------+--------------------+-----------+-----------------|
| imperial-star-destroyer.md | starwars/starships | 281       | 15              |
|----------------------------+--------------------+-----------+-----------------|
| slave-i.md                 | starwars/starships | 261       | 13              |
|----------------------------+--------------------+-----------+-----------------|
| tie-advanced-x1.md         | starwars/starships | 258       | 16              |
|----------------------------+--------------------+-----------+-----------------|
| executor.md                | starwars/starships | 243       | 12              |
+----------------------------+--------------------+-----------+-----------------+
```

## file.mtime is deterministic in generated trees

`starwars/` pins every mtime to `1977-05-25` — the release date of the
original film — so this query's output never changes between regenerations.

```sql
SELECT file.name, file.mtime FROM 'starwars/planets/**' ORDER BY file.name LIMIT 3;
```

```
+-------------+----------------------+
| file.name   | file.mtime           |
+====================================+
| alderaan.md | 1977-05-25T00:00:00Z |
|-------------+----------------------|
| bespin.md   | 1977-05-25T00:00:00Z |
|-------------+----------------------|
| dagobah.md  | 1977-05-25T00:00:00Z |
+-------------+----------------------+
```

## file.body is read lazily at query time

`file.body` is only read from disk for a query that actually references it.
Here `REGEXP` scans the Markdown body for `TODO`/`FIXME` markers.

```sql
SELECT file.name FROM 'work/**' WHERE file.body REGEXP 'TODO|FIXME' ORDER BY file.name LIMIT 5;
```

```
+----------------------------+
| file.name                  |
+============================+
| DCP-105-billing-metrics.md |
|----------------------------|
| DCP-106-export-export.md   |
|----------------------------|
| DCP-107-webhook-upload.md  |
|----------------------------|
| DCP-109-login-profile.md   |
|----------------------------|
| DCP-113-login-report.md    |
+----------------------------+
```

## Nested dotted paths walk into YAML mappings

`estimate.low`/`estimate.high` read into the `estimate:` mapping nested
under each work doc's frontmatter.

```sql
SELECT jira, estimate.low, estimate.high FROM 'work/plans/**' WHERE estimate.high > 12 ORDER BY jira LIMIT 5;
```

```
+---------+--------------+---------------+
| jira    | estimate.low | estimate.high |
+========================================+
| DCP-118 | 8            | 14            |
|---------+--------------+---------------|
| DCP-133 | 6            | 14            |
|---------+--------------+---------------|
| DCP-139 | 8            | 13            |
|---------+--------------+---------------|
| DCP-166 | 7            | 15            |
|---------+--------------+---------------|
| DCP-205 | 5            | 13            |
+---------+--------------+---------------+
```

## MEMBER OF: literal on the left

A quoted literal tests membership in a list-valued column — here, characters
who appeared in `EMPIRE` but not `NEWHOPE`.

```sql
SELECT name FROM 'starwars/characters/**' WHERE 'EMPIRE' MEMBER OF(episodes) AND NOT 'NEWHOPE' MEMBER OF(episodes) ORDER BY name;
```

```
+-------------------+
| name              |
+===================+
| Boba Fett         |
|-------------------|
| Emperor Palpatine |
|-------------------|
| IG-88             |
|-------------------|
| Lando Calrissian  |
|-------------------|
| Lobot             |
|-------------------|
| Yoda              |
+-------------------+
```

## MEMBER OF: a column on the left works too

```sql
SELECT jira, lead FROM 'work/**' WHERE lead MEMBER OF(reviewers) ORDER BY jira LIMIT 5;
```

```
+---------+--------------+
| jira    | lead         |
+========================+
| DCP-109 | Riley Brooks |
|---------+--------------|
| DCP-110 | Sam Rivera   |
|---------+--------------|
| DCP-113 | Quinn Foster |
|---------+--------------|
| DCP-118 | Morgan Lee   |
|---------+--------------|
| DCP-120 | Sam Rivera   |
+---------+--------------+
```

## LIKE with % wildcards

```sql
SELECT name FROM 'starwars/starships/**' WHERE manufacturer LIKE '%Kuat%' ORDER BY name;
```

```
+-------------------------+
| name                    |
+=========================+
| Executor                |
|-------------------------|
| Imperial Star Destroyer |
|-------------------------|
| Slave I                 |
+-------------------------+
```

## NOT LIKE excludes the same wildcard match

```sql
SELECT name FROM 'starwars/starships/**' WHERE manufacturer NOT LIKE '%Kuat%' ORDER BY name;
```

```
+-------------------+
| name              |
+===================+
| A-wing            |
|-------------------|
| Millennium Falcon |
|-------------------|
| TIE Advanced x1   |
|-------------------|
| X-wing            |
|-------------------|
| Y-wing            |
+-------------------+
```

## REGEXP against a computed expression

`REGEXP` isn't limited to a bare column — here it's matched against
`lower(title)`, so the pattern doesn't need to account for capitalization.

```sql
SELECT title, cuisine FROM 'recipes/**' WHERE lower(title) REGEXP 'chick(en|pea)' ORDER BY title LIMIT 5;
```

```
+-------------------------+---------+
| title                   | cuisine |
+===================================+
| Creamy Chicken Noodles  | indian  |
|-------------------------+---------|
| Creamy Chicken Stir-Fry | korean  |
|-------------------------+---------|
| Creamy Chickpea Curry   | thai    |
|-------------------------+---------|
| Creamy Chickpea Salad   | mexican |
|-------------------------+---------|
| Creamy Chickpea Soup    | indian  |
+-------------------------+---------+
```

## IN over a literal list

```sql
SELECT name, home_planet FROM 'starwars/characters/**' WHERE home_planet IN ('Tatooine', 'Naboo') ORDER BY name;
```

```
+-------------------+-------------+
| name              | home_planet |
+=================================+
| C-3PO             | Tatooine    |
|-------------------+-------------|
| Darth Vader       | Tatooine    |
|-------------------+-------------|
| Emperor Palpatine | Naboo       |
|-------------------+-------------|
| Luke Skywalker    | Tatooine    |
|-------------------+-------------|
| R2-D2             | Naboo       |
+-------------------+-------------+
```

## NOT IN excludes the same literal list

```sql
SELECT name, home_planet FROM 'starwars/characters/**' WHERE home_planet NOT IN ('Tatooine', 'Naboo') ORDER BY name LIMIT 5;
```

```
+----------------+-------------+
| name           | home_planet |
+==============================+
| Admiral Ackbar | Mon Cala    |
|----------------+-------------|
| Boba Fett      | Kamino      |
|----------------+-------------|
| Chewbacca      | Kashyyyk    |
|----------------+-------------|
| Greedo         | Rodia       |
|----------------+-------------|
| Han Solo       | Corellia    |
+----------------+-------------+
```

## IS NULL: absent frontmatter keys read as NULL

```sql
SELECT name, climate FROM 'starwars/planets/**' WHERE population IS NULL ORDER BY name;
```

```
+---------+---------+
| name    | climate |
+===================+
| Dagobah | murky   |
|---------+---------|
| Hoth    | frozen  |
+---------+---------+
```

## Scalar functions and aliases

```sql
SELECT upper(name) AS loud, length(name) AS len FROM 'starwars/characters/**' ORDER BY len DESC LIMIT 3;
```

```
+-------------------+-----+
| loud              | len |
+=========================+
| EMPEROR PALPATINE | 17  |
|-------------------+-----|
| LANDO CALRISSIAN  | 16  |
|-------------------+-----|
| ADMIRAL ACKBAR    | 14  |
+-------------------+-----+
```

## trim() strips padding, replace() swaps a substring

`replace(status, '-', ' ')` turns `in-review` into a friendlier `in review`;
`trim('  ' || jira || '  ')` strips the two spaces padded onto each side
before `length()` measures it — `trimmed_len` comes out equal to the bare
`jira`'s own length (7), proving the padding is gone.

```sql
SELECT jira, replace(status, '-', ' ') AS status_label, length(trim('  ' || jira || '  ')) AS trimmed_len FROM 'work/**' ORDER BY jira LIMIT 5;
```

```
+---------+--------------+-------------+
| jira    | status_label | trimmed_len |
+======================================+
| DCP-100 | in review    | 7           |
|---------+--------------+-------------|
| DCP-101 | in review    | 7           |
|---------+--------------+-------------|
| DCP-102 | draft        | 7           |
|---------+--------------+-------------|
| DCP-103 | done         | 7           |
|---------+--------------+-------------|
| DCP-104 | draft        | 7           |
+---------+--------------+-------------+
```

## String concatenation with ||

```sql
SELECT substr(name, 1, 8) || '...' AS clipped FROM 'starwars/starships/**' ORDER BY clipped LIMIT 4;
```

```
+-------------+
| clipped     |
+=============+
| A-wing...   |
|-------------|
| Executor... |
|-------------|
| Imperial... |
|-------------|
| Millenni... |
+-------------+
```

## Arithmetic in SELECT and WHERE

```sql
SELECT title, prep_minutes + cook_minutes AS total_minutes FROM 'recipes/**' WHERE prep_minutes + cook_minutes > 110 ORDER BY total_minutes DESC LIMIT 5;
```

```
+---------------------------+---------------+
| title                     | total_minutes |
+===========================================+
| Tangy Cauliflower Tacos   | 134           |
|---------------------------+---------------|
| Sweet Cauliflower Noodles | 131           |
|---------------------------+---------------|
| Spicy Lentil Curry        | 131           |
|---------------------------+---------------|
| Crispy Shrimp Stew        | 130           |
|---------------------------+---------------|
| Crispy Chicken Salad      | 129           |
+---------------------------+---------------+
```

## COALESCE picks the first non-null argument

`epic` is missing on some work docs; `COALESCE` falls back to a literal.

```sql
SELECT jira, COALESCE(epic, 'unassigned') AS epic FROM 'work/plans/**' ORDER BY jira LIMIT 5;
```

```
+---------+---------------+
| jira    | epic          |
+=========================+
| DCP-100 | unassigned    |
|---------+---------------|
| DCP-103 | mobile-parity |
|---------+---------------|
| DCP-106 | search-v2     |
|---------+---------------|
| DCP-109 | mobile-parity |
|---------+---------------|
| DCP-112 | mobile-parity |
+---------+---------------+
```

## Searched CASE

All three branches show up across the full 20-character cast — most
characters have a `mass_kg`, two don't (`unknown`).

```sql
SELECT name, CASE WHEN mass_kg IS NULL THEN 'unknown' WHEN mass_kg >= 100 THEN 'heavy' ELSE 'light' END AS build FROM 'starwars/characters/**' ORDER BY name;
```

```
+-------------------+---------+
| name              | build   |
+=============================+
| Admiral Ackbar    | light   |
|-------------------+---------|
| Boba Fett         | light   |
|-------------------+---------|
| C-3PO             | light   |
|-------------------+---------|
| Chewbacca         | heavy   |
|-------------------+---------|
| Darth Vader       | heavy   |
|-------------------+---------|
| Emperor Palpatine | light   |
|-------------------+---------|
| Greedo            | light   |
|-------------------+---------|
| Han Solo          | light   |
|-------------------+---------|
| IG-88             | heavy   |
|-------------------+---------|
| Jabba the Hutt    | heavy   |
|-------------------+---------|
| Lando Calrissian  | light   |
|-------------------+---------|
| Leia Organa       | light   |
|-------------------+---------|
| Lobot             | light   |
|-------------------+---------|
| Luke Skywalker    | light   |
|-------------------+---------|
| Mon Mothma        | unknown |
|-------------------+---------|
| Obi-Wan Kenobi    | light   |
|-------------------+---------|
| R2-D2             | light   |
|-------------------+---------|
| Wedge Antilles    | light   |
|-------------------+---------|
| Wilhuff Tarkin    | unknown |
|-------------------+---------|
| Yoda              | light   |
+-------------------+---------+
```

## Simple CASE

The *simple* form compares one expression (`status`) against each `WHEN`
value for equality, instead of a full condition per branch.

```sql
SELECT jira, CASE status WHEN 'blocked' THEN 'B' WHEN 'draft' THEN 'D' WHEN 'done' THEN 'X' ELSE 'other' END AS code FROM 'work/**' ORDER BY jira LIMIT 5;
```

```
+---------+-------+
| jira    | code  |
+=================+
| DCP-100 | other |
|---------+-------|
| DCP-101 | other |
|---------+-------|
| DCP-102 | D     |
|---------+-------|
| DCP-103 | X     |
|---------+-------|
| DCP-104 | D     |
+---------+-------+
```

## CASE as an ORDER BY expression

Sorting on a `CASE` expression, not just a column — blocked work sorts
first.

```sql
SELECT jira, status FROM 'work/**' ORDER BY CASE WHEN status = 'blocked' THEN 0 ELSE 1 END, jira LIMIT 5;
```

```
+---------+---------+
| jira    | status  |
+===================+
| DCP-110 | blocked |
|---------+---------|
| DCP-122 | blocked |
|---------+---------|
| DCP-137 | blocked |
|---------+---------|
| DCP-155 | blocked |
|---------+---------|
| DCP-167 | blocked |
+---------+---------+
```

## GROUP BY + count + ORDER BY the alias

```sql
SELECT status, count(*) AS n FROM 'work/**' GROUP BY status ORDER BY n DESC;
```

```
+-----------+-----+
| status    | n   |
+=================+
| draft     | 138 |
|-----------+-----|
| synced    | 122 |
|-----------+-----|
| done      | 90  |
|-----------+-----|
| in-review | 85  |
|-----------+-----|
| blocked   | 48  |
+-----------+-----+
```

## Aggregates with HAVING on an alias

`HAVING` filters *groups*, after aggregation — here, only cuisines with at
least 35 recipes.

```sql
SELECT cuisine, count(*) AS n, avg(prep_minutes) AS avg_prep FROM 'recipes/**' GROUP BY cuisine HAVING n >= 35 ORDER BY n DESC;
```

```
+----------+----+--------------------+
| cuisine  | n  | avg_prep           |
+====================================+
| mexican  | 45 | 24.533333333333335 |
|----------+----+--------------------|
| korean   | 38 | 23.86842105263158  |
|----------+----+--------------------|
| greek    | 37 | 23.486486486486488 |
|----------+----+--------------------|
| french   | 35 | 23.17142857142857  |
|----------+----+--------------------|
| japanese | 35 | 27.571428571428573 |
|----------+----+--------------------|
| thai     | 35 | 27.057142857142857 |
+----------+----+--------------------+
```

## min / max / sum without GROUP BY

```sql
SELECT min(height_cm) AS shortest, max(height_cm) AS tallest, sum(mass_kg) AS total_mass FROM 'starwars/characters/**';
```

```
+----------+---------+------------+
| shortest | tallest | total_mass |
+=================================+
| 66       | 228     | 2698       |
+----------+---------+------------+
```

## count(distinct col)

```sql
SELECT count(distinct author) AS authors FROM 'reading/**';
```

```
+---------+
| authors |
+=========+
| 10      |
+---------+
```

## group_concat

```sql
SELECT kind, group_concat(name) AS members FROM 'starwars/characters/**' GROUP BY kind ORDER BY kind;
```

```
+---------+-----------------------------------------------------------------------------------------------------------------------------------------------------------------------+
| kind    | members                                                                                                                                                               |
+=================================================================================================================================================================================+
| droid   | C-3PO, IG-88, R2-D2                                                                                                                                                   |
|---------+-----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| human   | Boba Fett, Darth Vader, Emperor Palpatine, Han Solo, Lando Calrissian, Leia Organa, Lobot, Luke Skywalker, Mon Mothma, Obi-Wan Kenobi, Wedge Antilles, Wilhuff Tarkin |
|---------+-----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| hutt    | Jabba the Hutt                                                                                                                                                        |
|---------+-----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| other   | Admiral Ackbar, Greedo, Yoda                                                                                                                                          |
|---------+-----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| wookiee | Chewbacca                                                                                                                                                             |
+---------+-----------------------------------------------------------------------------------------------------------------------------------------------------------------------+
```

## Auto-detected ISO dates compare chronologically

`created` is a strict ISO date, so `>=` compares calendar order, not string
order.

```sql
SELECT count(*) AS created_2026 FROM 'work/**' WHERE created >= '2026-01-01';
```

```
+--------------+
| created_2026 |
+==============+
| 168          |
+--------------+
```

## DATE() with no format argument

`created` is already a strict ISO date, so `DATE(created)` with no format
argument casts it via the same detection ingest applies (strict `%Y-%m-%d`,
then RFC3339) — an already-`Date` value like this one just passes through.

```sql
SELECT jira, DATE(created) AS created_on FROM 'work/**' ORDER BY jira LIMIT 5;
```

```
+---------+------------+
| jira    | created_on |
+======================+
| DCP-100 | 2025-08-06 |
|---------+------------|
| DCP-101 | 2025-02-25 |
|---------+------------|
| DCP-102 | 2026-06-13 |
|---------+------------|
| DCP-103 | 2026-06-09 |
|---------+------------|
| DCP-104 | 2026-03-17 |
+---------+------------+
```

## DATE() with an explicit chrono format

`purchased` is stored as `MM/DD/YYYY`, not ISO — `DATE(x, fmt)` parses it
against an explicit [chrono strftime
pattern](https://docs.rs/chrono/latest/chrono/format/strftime/index.html).

```sql
SELECT title, DATE(purchased, '%m/%d/%Y') AS purchased_on FROM 'reading/2026/**' WHERE purchased IS NOT NULL ORDER BY purchased_on LIMIT 5;
```

```
+---------------------+--------------+
| title               | purchased_on |
+====================================+
| The Hollow Atlas    | 2026-01-11   |
|---------------------+--------------|
| The Hollow Voyage   | 2026-03-16   |
|---------------------+--------------|
| The Endless Kingdom | 2026-04-07   |
|---------------------+--------------|
| The Burning Orchard | 2026-05-12   |
|---------------------+--------------|
| The Endless Orchard | 2026-06-04   |
+---------------------+--------------+
```

## ORDER BY a bare scalar fn needs parens

A bare, top-level `ORDER BY upper(name)` is tried as an aggregate first and
rejected — wrap it in parentheses (or alias it in `SELECT`) instead. See
[Boundaries worth knowing](../README.md#boundaries-worth-knowing).

```sql
SELECT name FROM 'starwars/planets/**' ORDER BY (upper(name)) LIMIT 3;
```

```
+----------+
| name     |
+==========+
| Alderaan |
|----------|
| Bespin   |
|----------|
| Dagobah  |
+----------+
```

## LIMIT/OFFSET pagination

```sql
SELECT name FROM 'starwars/characters/**' ORDER BY name LIMIT 5 OFFSET 5;
```

```
+-------------------+
| name              |
+===================+
| Emperor Palpatine |
|-------------------|
| Greedo            |
|-------------------|
| Han Solo          |
|-------------------|
| IG-88             |
|-------------------|
| Jabba the Hutt    |
+-------------------+
```

## \G renders one row as name: value lines

Great for wide rows — the whole record prints as a block of `name: value`
lines instead of a cramped table.

```sql
SELECT * FROM 'starwars/planets/**' WHERE name = 'Dagobah'\G
```

```
*************************** 1. row ***************************
  climate: murky
     name: Dagobah
residents: Yoda
  terrain: swamp, jungles
```

## Relative-date literals (time-dependent)

These resolve against the clock at query time, so their results depend on
when you run them — they're not in `sample-queries.sql` (whose output is
pinned by a test):

```sql
SELECT jira, updated FROM 'work/**' WHERE updated >= '-6mo' ORDER BY updated DESC LIMIT 5
SELECT count(*) AS overdue FROM 'work/**' WHERE due < 'today'
```

## Erroring on purpose: unknown-column validation

A typo'd column name is a hard error by default, naming the offending
column and suggesting the nearest real one:

```console
$ querymatter -e "SELECT staus" samples
querymatter: failed to execute query: SELECT staus: unknown column `staus`, did you mean 'status'?
```

## Other output formats

```sh
querymatter -e "SELECT name, climate FROM 'starwars/planets/**'" --format json samples | jq '.[0]'
querymatter -e "SELECT status, count(*) AS n FROM 'work/**' GROUP BY status" --format csv samples
```

## Testing at scale

```sh
cargo run --release --bin querymatter-samples -- --scale 100k --force samples
time querymatter -e "SELECT status, count(*) AS n FROM 'work/**' GROUP BY status ORDER BY n DESC" samples
querymatter init samples        # build the .querymatter cache
time querymatter -e "SELECT status, count(*) AS n FROM 'work/**' GROUP BY status ORDER BY n DESC" samples
```
