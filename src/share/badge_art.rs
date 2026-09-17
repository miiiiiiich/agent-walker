//! Original art generated for this project, embedded to avoid network or filesystem access.
//! `currentColor` lets one asset serve every OPS tint.

pub(crate) fn badge_inner(animal: &str) -> Option<&'static str> {
    let svg = match animal {
        "Hound" => include_str!("../../assets/badges/hound.svg"),
        "Fox" => include_str!("../../assets/badges/fox.svg"),
        "Doberman" => include_str!("../../assets/badges/doberman.svg"),
        "Lion" => include_str!("../../assets/badges/lion.svg"),
        "Octopus" => include_str!("../../assets/badges/octopus.svg"),
        "Wolf" => include_str!("../../assets/badges/wolf.svg"),
        "Orca" => include_str!("../../assets/badges/orca.svg"),
        "Hawk" => include_str!("../../assets/badges/hawk.svg"),
        "Raven" => include_str!("../../assets/badges/raven.svg"),
        "Eel" => include_str!("../../assets/badges/eel.svg"),
        "Whale" => include_str!("../../assets/badges/whale.svg"),
        "Swallow" => include_str!("../../assets/badges/swallow.svg"),
        "Scorpion" => include_str!("../../assets/badges/scorpion.svg"),
        "Piranha" => include_str!("../../assets/badges/piranha.svg"),
        "Bear" => include_str!("../../assets/badges/bear.svg"),
        "Gull" => include_str!("../../assets/badges/gull.svg"),
        "Cat" => include_str!("../../assets/badges/cat.svg"),
        "Kangaroo" => include_str!("../../assets/badges/kangaroo.svg"),
        "Puma" => include_str!("../../assets/badges/puma.svg"),
        "Deer" => include_str!("../../assets/badges/deer.svg"),
        "Ant" => include_str!("../../assets/badges/ant.svg"),
        "Firefly" => include_str!("../../assets/badges/firefly.svg"),
        "Butterfly" => include_str!("../../assets/badges/butterfly.svg"),
        "Bee" => include_str!("../../assets/badges/bee.svg"),
        _ => return None,
    };
    Some(svg)
}
