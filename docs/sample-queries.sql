-- querymatter sample queries — run against a generated sample tree:
--   cargo run --bin querymatter-samples -- --scale 1k samples
--   querymatter samples < docs/sample-queries.sql
-- Each statement below is explained in docs/sample-queries.md.

-- The whole tree: exactly the --scale you generated
SELECT count(*) AS total;

-- Basic SELECT + WHERE with a numeric comparison
SELECT name, height_cm FROM 'starwars/characters/**' WHERE height_cm > 180 ORDER BY height_cm DESC;

-- String equality WHERE (quoted literal = string comparison)
SELECT name, primary_function FROM 'starwars/characters/**' WHERE kind = 'droid' ORDER BY name;

-- SELECT * — every frontmatter key seen, sorted
SELECT * FROM 'starwars/planets/**' ORDER BY name LIMIT 3;

-- DISTINCT drops duplicate projected rows
SELECT DISTINCT affiliation FROM 'starwars/characters/**' ORDER BY affiliation;

-- file.* pseudo-columns come from the path and stat, not frontmatter
SELECT file.name, file.folder, file.size, file.word_count FROM 'starwars/starships/**' ORDER BY file.size DESC LIMIT 5;

-- file.mtime is deterministic in generated trees (starwars pins 1977-05-25)
SELECT file.name, file.mtime FROM 'starwars/planets/**' ORDER BY file.name LIMIT 3;

-- file.body is read lazily at query time — REGEXP scans it
SELECT file.name FROM 'work/**' WHERE file.body REGEXP 'TODO|FIXME' ORDER BY file.name LIMIT 5;

-- Nested dotted paths walk into YAML mappings
SELECT jira, estimate.low, estimate.high FROM 'work/plans/**' WHERE estimate.high > 12 ORDER BY jira LIMIT 5;

-- A second nested-path example: series.name/series.book (about 1 in 4 reading records has a series)
SELECT title, series.name, series.book FROM 'reading/**' WHERE series.name IS NOT NULL ORDER BY series.name, series.book LIMIT 5;

-- MEMBER OF: literal on the left, list-valued column on the right
SELECT name FROM 'starwars/characters/**' WHERE 'EMPIRE' MEMBER OF(episodes) AND NOT 'NEWHOPE' MEMBER OF(episodes) ORDER BY name;

-- MEMBER OF: a column on the left works too
SELECT jira, lead FROM 'work/**' WHERE lead MEMBER OF(reviewers) ORDER BY jira LIMIT 5;

-- LIKE with % wildcards
SELECT name FROM 'starwars/starships/**' WHERE manufacturer LIKE '%Kuat%' ORDER BY name;

-- NOT LIKE excludes the same wildcard match instead of including it
SELECT name FROM 'starwars/starships/**' WHERE manufacturer NOT LIKE '%Kuat%' ORDER BY name;

-- REGEXP against a computed expression, not just a bare column
SELECT title, cuisine FROM 'recipes/**' WHERE lower(title) REGEXP 'chick(en|pea)' ORDER BY title LIMIT 5;

-- IN over a literal list
SELECT name, home_planet FROM 'starwars/characters/**' WHERE home_planet IN ('Tatooine', 'Naboo') ORDER BY name;

-- NOT IN excludes the same literal list instead of including it
SELECT name, home_planet FROM 'starwars/characters/**' WHERE home_planet NOT IN ('Tatooine', 'Naboo') ORDER BY name LIMIT 5;

-- IS NULL: absent frontmatter keys read as NULL
SELECT name, climate FROM 'starwars/planets/**' WHERE population IS NULL ORDER BY name;

-- Scalar functions and aliases
SELECT upper(name) AS loud, length(name) AS len FROM 'starwars/characters/**' ORDER BY len DESC LIMIT 3;

-- trim() strips padding — replace() swaps a substring
SELECT jira, replace(status, '-', ' ') AS status_label, length(trim('  ' || jira || '  ')) AS trimmed_len FROM 'work/**' ORDER BY jira LIMIT 5;

-- String concatenation with ||
SELECT substr(name, 1, 8) || '...' AS clipped FROM 'starwars/starships/**' ORDER BY clipped LIMIT 4;

-- Arithmetic in SELECT and WHERE
SELECT title, prep_minutes + cook_minutes AS total_minutes FROM 'recipes/**' WHERE prep_minutes + cook_minutes > 110 ORDER BY total_minutes DESC LIMIT 5;

-- Arithmetic beyond +: subtraction, multiplication, division, modulo
SELECT title, cook_minutes - prep_minutes AS active_diff, prep_minutes * 2 AS doubled_prep, cook_minutes / 2 AS half_cook, servings % 2 AS servings_parity FROM 'recipes/**' ORDER BY title LIMIT 5;

-- COALESCE picks the first non-null argument
SELECT jira, COALESCE(epic, 'unassigned') AS epic FROM 'work/plans/**' ORDER BY jira LIMIT 5;

-- Searched CASE (all three branches show up across the full 20-character cast)
SELECT name, CASE WHEN mass_kg IS NULL THEN 'unknown' WHEN mass_kg >= 100 THEN 'heavy' ELSE 'light' END AS build FROM 'starwars/characters/**' ORDER BY name;

-- Simple CASE: expr compared against each WHEN value for equality
SELECT jira, CASE status WHEN 'blocked' THEN 'B' WHEN 'draft' THEN 'D' WHEN 'done' THEN 'X' ELSE 'other' END AS code FROM 'work/**' ORDER BY jira LIMIT 5;

-- CASE as an ORDER BY expression: blocked work first
SELECT jira, status FROM 'work/**' ORDER BY CASE WHEN status = 'blocked' THEN 0 ELSE 1 END, jira LIMIT 5;

-- GROUP BY + count + ORDER BY the alias
SELECT status, count(*) AS n FROM 'work/**' GROUP BY status ORDER BY n DESC;

-- Aggregates with HAVING on an alias
SELECT cuisine, count(*) AS n, avg(prep_minutes) AS avg_prep FROM 'recipes/**' GROUP BY cuisine HAVING n >= 35 ORDER BY n DESC;

-- GROUP BY a SELECT alias key, HAVING on both an aggregate and a key-column leaf, ORDER BY a bare aggregate
SELECT status AS s, count(*) AS n FROM 'work/**' GROUP BY s HAVING count(*) >= 35 AND s != 'draft' ORDER BY count(*) DESC;

-- min / max / sum without GROUP BY
SELECT min(height_cm) AS shortest, max(height_cm) AS tallest, sum(mass_kg) AS total_mass FROM 'starwars/characters/**';

-- count(col) only counts non-null rows — many reading records are unrated
SELECT count(rating) AS rated FROM 'reading/**';

-- count(distinct col)
SELECT count(distinct author) AS authors FROM 'reading/**';

-- group_concat
SELECT kind, group_concat(name) AS members FROM 'starwars/characters/**' GROUP BY kind ORDER BY kind;

-- Auto-detected ISO dates compare chronologically
SELECT count(*) AS created_2026 FROM 'work/**' WHERE created >= '2026-01-01';

-- DATE() with no format argument: strict ISO / RFC3339 detection, same as ingest
SELECT jira, DATE(created) AS created_on FROM 'work/**' ORDER BY jira LIMIT 5;

-- DATE() with an explicit chrono format parses non-ISO strings
SELECT title, DATE(purchased, '%m/%d/%Y') AS purchased_on FROM 'reading/2026/**' WHERE purchased IS NOT NULL ORDER BY purchased_on LIMIT 5;

-- ORDER BY a bare scalar fn needs parens (or an alias)
SELECT name FROM 'starwars/planets/**' ORDER BY (upper(name)) LIMIT 3;

-- LIMIT/OFFSET pagination
SELECT name FROM 'starwars/characters/**' ORDER BY name LIMIT 5 OFFSET 5;

-- A trailing backslash-G renders one row as name: value lines (great for wide rows)
SELECT * FROM 'starwars/planets/**' WHERE name = 'Dagobah'\G
