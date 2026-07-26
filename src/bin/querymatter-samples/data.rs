//! Hand-authored sample data tables. Star-wars entities are fixed at every
//! scale; edits here intentionally change generated output (and snapshots).

// This module's full public surface is the Task 3 interface contract that
// later tasks in the sample-generator plan consume (see
// docs/superpowers/plans/2026-07-26-sample-generator.md); `main()` doesn't
// call into it until Task 6 wires the generator pipeline together. The
// tests in `starwars` already exercise these tables. Drop this once
// Task 6 lands and every item has a real caller.
#![allow(dead_code)]

pub struct Character {
    pub name: &'static str,
    pub kind: &'static str,
    pub episodes: &'static [&'static str],
    pub friends: &'static [&'static str],
    pub home_planet: &'static str,
    pub height_cm: u32,
    pub mass_kg: Option<u32>,
    pub primary_function: Option<&'static str>,
    pub affiliation: &'static str,
}

pub struct Starship {
    pub name: &'static str,
    pub model: &'static str,
    pub manufacturer: &'static str,
    pub crew: u32,
    pub hyperdrive_rating: &'static str, // written verbatim — float determinism
    pub pilots: &'static [&'static str],
    pub episodes: &'static [&'static str],
}

pub struct Planet {
    pub name: &'static str,
    pub climate: &'static str,
    pub terrain: &'static str,
    pub population: Option<u64>,
    pub residents: &'static [&'static str],
}

const TRILOGY: &[&str] = &["NEWHOPE", "EMPIRE", "JEDI"];

pub const CHARACTERS: [Character; 20] = [
    Character {
        name: "Luke Skywalker",
        kind: "human",
        episodes: TRILOGY,
        friends: &["Han Solo", "Leia Organa", "C-3PO", "R2-D2"],
        home_planet: "Tatooine",
        height_cm: 172,
        mass_kg: Some(77),
        primary_function: None,
        affiliation: "Rebel Alliance",
    },
    Character {
        name: "Darth Vader",
        kind: "human",
        episodes: TRILOGY,
        friends: &["Wilhuff Tarkin"],
        home_planet: "Tatooine",
        height_cm: 202,
        mass_kg: Some(136),
        primary_function: None,
        affiliation: "Galactic Empire",
    },
    Character {
        name: "Han Solo",
        kind: "human",
        episodes: TRILOGY,
        friends: &["Luke Skywalker", "Leia Organa", "R2-D2", "Chewbacca"],
        home_planet: "Corellia",
        height_cm: 180,
        mass_kg: Some(80),
        primary_function: None,
        affiliation: "Rebel Alliance",
    },
    Character {
        name: "Leia Organa",
        kind: "human",
        episodes: TRILOGY,
        friends: &["Luke Skywalker", "Han Solo", "C-3PO", "R2-D2"],
        home_planet: "Alderaan",
        height_cm: 150,
        mass_kg: Some(49),
        primary_function: None,
        affiliation: "Rebel Alliance",
    },
    Character {
        name: "Wilhuff Tarkin",
        kind: "human",
        episodes: &["NEWHOPE"],
        friends: &["Darth Vader"],
        home_planet: "Eriadu",
        height_cm: 180,
        mass_kg: None,
        primary_function: None,
        affiliation: "Galactic Empire",
    },
    Character {
        name: "C-3PO",
        kind: "droid",
        episodes: TRILOGY,
        friends: &["Luke Skywalker", "Han Solo", "Leia Organa", "R2-D2"],
        home_planet: "Tatooine",
        height_cm: 167,
        mass_kg: Some(75),
        primary_function: Some("Protocol"),
        affiliation: "Rebel Alliance",
    },
    Character {
        name: "R2-D2",
        kind: "droid",
        episodes: TRILOGY,
        friends: &["Luke Skywalker", "Han Solo", "Leia Organa"],
        home_planet: "Naboo",
        height_cm: 96,
        mass_kg: Some(32),
        primary_function: Some("Astromech"),
        affiliation: "Rebel Alliance",
    },
    Character {
        name: "Obi-Wan Kenobi",
        kind: "human",
        episodes: TRILOGY,
        friends: &["Luke Skywalker", "Yoda"],
        home_planet: "Stewjon",
        height_cm: 182,
        mass_kg: Some(77),
        primary_function: None,
        affiliation: "Jedi Order",
    },
    Character {
        name: "Yoda",
        kind: "other",
        episodes: &["EMPIRE", "JEDI"],
        friends: &["Obi-Wan Kenobi", "Luke Skywalker"],
        home_planet: "Dagobah",
        height_cm: 66,
        mass_kg: Some(17),
        primary_function: None,
        affiliation: "Jedi Order",
    },
    Character {
        name: "Chewbacca",
        kind: "wookiee",
        episodes: TRILOGY,
        friends: &["Han Solo", "Luke Skywalker", "Leia Organa"],
        home_planet: "Kashyyyk",
        height_cm: 228,
        mass_kg: Some(112),
        primary_function: None,
        affiliation: "Rebel Alliance",
    },
    Character {
        name: "Lando Calrissian",
        kind: "human",
        episodes: &["EMPIRE", "JEDI"],
        friends: &["Han Solo", "Chewbacca"],
        home_planet: "Socorro",
        height_cm: 177,
        mass_kg: Some(79),
        primary_function: None,
        affiliation: "Rebel Alliance",
    },
    Character {
        name: "Emperor Palpatine",
        kind: "human",
        episodes: &["EMPIRE", "JEDI"],
        friends: &["Darth Vader"],
        home_planet: "Naboo",
        height_cm: 170,
        mass_kg: Some(75),
        primary_function: None,
        affiliation: "Galactic Empire",
    },
    Character {
        name: "Boba Fett",
        kind: "human",
        episodes: &["EMPIRE", "JEDI"],
        friends: &[],
        home_planet: "Kamino",
        height_cm: 183,
        mass_kg: Some(78),
        primary_function: None,
        affiliation: "Bounty Hunters Guild",
    },
    Character {
        name: "Jabba the Hutt",
        kind: "hutt",
        episodes: &["NEWHOPE", "JEDI"],
        friends: &["Boba Fett"],
        home_planet: "Nal Hutta",
        height_cm: 175,
        mass_kg: Some(1358),
        primary_function: None,
        affiliation: "Hutt Cartel",
    },
    Character {
        name: "Wedge Antilles",
        kind: "human",
        episodes: TRILOGY,
        friends: &["Luke Skywalker"],
        home_planet: "Corellia",
        height_cm: 170,
        mass_kg: Some(77),
        primary_function: None,
        affiliation: "Rebel Alliance",
    },
    Character {
        name: "Admiral Ackbar",
        kind: "other",
        episodes: &["JEDI"],
        friends: &["Mon Mothma"],
        home_planet: "Mon Cala",
        height_cm: 180,
        mass_kg: Some(83),
        primary_function: None,
        affiliation: "Rebel Alliance",
    },
    Character {
        name: "Mon Mothma",
        kind: "human",
        episodes: &["JEDI"],
        friends: &["Admiral Ackbar", "Leia Organa"],
        home_planet: "Chandrila",
        height_cm: 150,
        mass_kg: None,
        primary_function: None,
        affiliation: "Rebel Alliance",
    },
    Character {
        name: "Greedo",
        kind: "other",
        episodes: &["NEWHOPE"],
        friends: &[],
        home_planet: "Rodia",
        height_cm: 173,
        mass_kg: Some(74),
        primary_function: None,
        affiliation: "Hutt Cartel",
    },
    Character {
        name: "Lobot",
        kind: "human",
        episodes: &["EMPIRE"],
        friends: &["Lando Calrissian"],
        home_planet: "Bespin",
        height_cm: 175,
        mass_kg: Some(79),
        primary_function: None,
        affiliation: "Cloud City",
    },
    Character {
        name: "IG-88",
        kind: "droid",
        episodes: &["EMPIRE"],
        friends: &[],
        home_planet: "Holowan",
        height_cm: 200,
        mass_kg: Some(140),
        primary_function: Some("Assassin"),
        affiliation: "Bounty Hunters Guild",
    },
];

pub const STARSHIPS: [Starship; 8] = [
    Starship {
        name: "Millennium Falcon",
        model: "YT-1300 light freighter",
        manufacturer: "Corellian Engineering Corporation",
        crew: 4,
        hyperdrive_rating: "0.5",
        pilots: &["Han Solo", "Chewbacca", "Lando Calrissian"],
        episodes: TRILOGY,
    },
    Starship {
        name: "X-wing",
        model: "T-65 X-wing",
        manufacturer: "Incom Corporation",
        crew: 1,
        hyperdrive_rating: "1.0",
        pilots: &["Luke Skywalker", "Wedge Antilles"],
        episodes: TRILOGY,
    },
    Starship {
        name: "TIE Advanced x1",
        model: "Twin Ion Engine Advanced x1",
        manufacturer: "Sienar Fleet Systems",
        crew: 1,
        hyperdrive_rating: "1.0",
        pilots: &["Darth Vader"],
        episodes: &["NEWHOPE"],
    },
    Starship {
        name: "Imperial Star Destroyer",
        model: "Imperial I-class Star Destroyer",
        manufacturer: "Kuat Drive Yards",
        crew: 47060,
        hyperdrive_rating: "2.0",
        pilots: &[],
        episodes: TRILOGY,
    },
    Starship {
        name: "Slave I",
        model: "Firespray-31-class patrol craft",
        manufacturer: "Kuat Systems Engineering",
        crew: 1,
        hyperdrive_rating: "3.0",
        pilots: &["Boba Fett"],
        episodes: &["EMPIRE", "JEDI"],
    },
    Starship {
        name: "Y-wing",
        model: "BTL Y-wing",
        manufacturer: "Koensayr Manufacturing",
        crew: 2,
        hyperdrive_rating: "1.0",
        pilots: &[],
        episodes: &["NEWHOPE", "JEDI"],
    },
    Starship {
        name: "A-wing",
        model: "RZ-1 A-wing interceptor",
        manufacturer: "Alliance Underground Engineering",
        crew: 1,
        hyperdrive_rating: "1.0",
        pilots: &[],
        episodes: &["JEDI"],
    },
    Starship {
        name: "Executor",
        model: "Executor-class Star Dreadnought",
        manufacturer: "Kuat Drive Yards",
        crew: 279144,
        hyperdrive_rating: "2.0",
        pilots: &[],
        episodes: &["EMPIRE", "JEDI"],
    },
];

pub const PLANETS: [Planet; 7] = [
    Planet {
        name: "Tatooine",
        climate: "arid",
        terrain: "desert",
        population: Some(200000),
        residents: &["Luke Skywalker", "Darth Vader", "C-3PO"],
    },
    Planet {
        name: "Alderaan",
        climate: "temperate",
        terrain: "grasslands, mountains",
        population: Some(2000000000),
        residents: &["Leia Organa"],
    },
    Planet {
        name: "Hoth",
        climate: "frozen",
        terrain: "tundra, ice caves",
        population: None,
        residents: &[],
    },
    Planet {
        name: "Dagobah",
        climate: "murky",
        terrain: "swamp, jungles",
        population: None,
        residents: &["Yoda"],
    },
    Planet {
        name: "Bespin",
        climate: "temperate",
        terrain: "gas giant",
        population: Some(6000000),
        residents: &["Lando Calrissian", "Lobot"],
    },
    Planet {
        name: "Endor",
        climate: "temperate",
        terrain: "forests, mountains",
        population: Some(30000000),
        residents: &[],
    },
    Planet {
        name: "Yavin IV",
        climate: "temperate, tropical",
        terrain: "jungle, rainforests",
        population: Some(1000),
        residents: &[],
    },
];

// ---- shared/theme pools (plain scalars only: no ':', '#', quotes) ----

pub const NAMES: [&str; 12] = [
    "Avery Chen",
    "Jordan Patel",
    "Sam Rivera",
    "Morgan Lee",
    "Casey Nguyen",
    "Riley Brooks",
    "Quinn Foster",
    "Alex Murphy",
    "Dana Kim",
    "Jesse Ortiz",
    "Robin Walsh",
    "Taylor Singh",
];

pub const WORK_TAGS: [&str; 8] = [
    "mobile", "web", "api", "infra", "ux", "docs", "security", "perf",
];

pub const EPICS: [&str; 6] = [
    "checkout-revamp",
    "search-v2",
    "mobile-parity",
    "billing-cleanup",
    "onboarding-flow",
    "data-platform",
];

pub const SLUG_WORDS: [&str; 16] = [
    "login", "cache", "export", "sync", "audit", "metrics", "retry", "webhook", "profile",
    "search", "billing", "notify", "upload", "archive", "session", "report",
];

pub const SENTENCES: [&str; 10] = [
    "The current behavior diverges from the design doc in two places.",
    "We agreed to gate this behind a feature flag until QA signs off.",
    "Latency regressions show up only under concurrent writes.",
    "The migration needs a rollback path before it can ship.",
    "Error handling swallows the root cause and logs a generic message.",
    "The retry loop should back off exponentially instead of hammering the API.",
    "Customer feedback suggests the empty state is confusing.",
    "This depends on the platform team exposing a stable endpoint.",
    "Old clients will keep sending the legacy payload for a while.",
    "The dashboard should surface the failure count per tenant.",
];

pub const CUISINES: [&str; 8] = [
    "italian", "thai", "mexican", "japanese", "indian", "french", "greek", "korean",
];

pub const DISH_ADJ: [&str; 8] = [
    "Spicy", "Creamy", "Crispy", "Smoky", "Sweet", "Tangy", "Herbed", "Roasted",
];

pub const DISH_ING: [&str; 12] = [
    "Chicken",
    "Chickpea",
    "Tofu",
    "Beef",
    "Mushroom",
    "Salmon",
    "Eggplant",
    "Lentil",
    "Shrimp",
    "Paneer",
    "Pork",
    "Cauliflower",
];

pub const DISH_FORM: [&str; 8] = [
    "Curry", "Stir-Fry", "Tacos", "Soup", "Salad", "Noodles", "Skewers", "Stew",
];

pub const INGREDIENTS: [&str; 16] = [
    "garlic",
    "onion",
    "ginger",
    "soy sauce",
    "olive oil",
    "cumin",
    "basil",
    "lime",
    "coconut milk",
    "tomatoes",
    "rice",
    "chili flakes",
    "yogurt",
    "cilantro",
    "sesame oil",
    "paprika",
];

pub const RECIPE_TAGS: [&str; 7] = [
    "vegetarian",
    "spicy",
    "quick",
    "weeknight",
    "gluten-free",
    "grill",
    "comfort",
];

pub const STEPS: [&str; 8] = [
    "Heat the oil in a large pan over medium heat.",
    "Add the aromatics and cook until fragrant.",
    "Stir in the main ingredient and sear on all sides.",
    "Deglaze with a splash of stock and scrape up the fond.",
    "Simmer until the sauce thickens slightly.",
    "Season to taste and adjust the acidity.",
    "Rest for five minutes off the heat.",
    "Garnish and serve immediately.",
];

pub const AUTHORS: [&str; 10] = [
    "Iris Malloy",
    "Theo Grant",
    "Nadia Osei",
    "Felix Aran",
    "June Park",
    "Marco Silva",
    "Priya Nair",
    "Owen Blake",
    "Zara Holt",
    "Ken Watanabe",
];

pub const TITLE_ADJ: [&str; 10] = [
    "Silent", "Burning", "Hidden", "Broken", "Endless", "Glass", "Iron", "Hollow", "Distant",
    "Golden",
];

pub const TITLE_NOUN: [&str; 12] = [
    "Harbor", "Empire", "Garden", "Cipher", "Mountain", "Archive", "Voyage", "Orchard", "Signal",
    "Kingdom", "Meridian", "Atlas",
];

pub const GENRES: [&str; 8] = [
    "sci-fi",
    "fantasy",
    "mystery",
    "history",
    "biography",
    "thriller",
    "essays",
    "poetry",
];

pub const SERIES: [&str; 5] = [
    "The Meridian Cycle",
    "Archive Wars",
    "The Glass Chronicles",
    "Signal and Noise",
    "Kingdom of Ash",
];

pub const NOTES: [&str; 6] = [
    "The pacing drags in the middle but the ending lands.",
    "Great worldbuilding with a memorable narrator.",
    "Read this for book club and it split the room.",
    "The research shows without smothering the story.",
    "A reread — holds up better than expected.",
    "Picked up on a recommendation from a colleague.",
];
