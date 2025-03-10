use self::{nx::Nx, turborepo::Turborepo};
use super::Provider;
use crate::nixpacks::{
    app::App,
    environment::{Environment, EnvironmentVariables},
    nix::pkg::Pkg,
    plan::{
        phase::{Phase, StartPhase},
        BuildPlan,
    },
};
use anyhow::Result;
use path_slash::PathExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

mod nx;
mod turborepo;

pub const NODE_OVERLAY: &str = "https://github.com/railwayapp/nix-npm-overlay/archive/main.tar.gz";

const DEFAULT_NODE_PKG_NAME: &str = "nodejs-16_x";
const AVAILABLE_NODE_VERSIONS: &[u32] = &[14, 16, 18];

const YARN_CACHE_DIR: &str = "/usr/local/share/.cache/yarn/v6";
const PNPM_CACHE_DIR: &str = "/root/.cache/pnpm";
const NPM_CACHE_DIR: &str = "/root/.npm";
const BUN_CACHE_DIR: &str = "/root/.bun";
const CYPRESS_CACHE_DIR: &str = "/root/.cache/Cypress";
const NODE_MODULES_CACHE_DIR: &str = "node_modules/.cache";

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum Workspaces {
    Array(Vec<String>),
    Unknown(Value),
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct PackageJson {
    pub name: Option<String>,
    pub scripts: Option<HashMap<String, String>>,
    pub engines: Option<HashMap<String, String>>,
    pub main: Option<String>,
    pub dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "devDependencies")]
    pub dev_dependencies: Option<HashMap<String, String>>,
    #[serde(rename = "type")]
    pub project_type: Option<String>,

    pub workspaces: Option<Workspaces>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Yarnrc {
    #[serde(rename = "yarnPath")]
    pub yarn_path: Option<String>,
}

#[derive(Default, Debug)]
pub struct NodeProvider {}

impl Provider for NodeProvider {
    fn name(&self) -> &str {
        "node"
    }

    fn detect(&self, app: &App, _env: &Environment) -> Result<bool> {
        Ok(app.includes_file("package.json"))
    }

    fn get_build_plan(&self, app: &App, env: &Environment) -> Result<Option<BuildPlan>> {
        // Setup
        let mut setup = Phase::setup(Some(NodeProvider::get_nix_packages(app, env)?));

        if NodeProvider::uses_node_dependency(app, "puppeteer") {
            // https://gist.github.com/winuxue/cfef08e2f5fe9dfc16a1d67a4ad38a01
            setup.add_apt_pkgs(vec![
                "libnss3".to_string(),
                "libatk1.0-0".to_string(),
                "libatk-bridge2.0-0".to_string(),
                "libcups2".to_string(),
                "libgbm1".to_string(),
                "libasound2".to_string(),
                "libpangocairo-1.0-0".to_string(),
                "libxss1".to_string(),
                "libgtk-3-0".to_string(),
                "libxshmfence1".to_string(),
                "libglu1".to_string(),
            ]);
        } else if NodeProvider::uses_node_dependency(app, "canvas") {
            setup.add_pkgs_libs(vec!["libuuid".to_string(), "libGL".to_string()]);
        }

        // Install
        let mut install = Phase::install(NodeProvider::get_install_command(app));
        install.add_cache_directory(NodeProvider::get_package_manager_cache_dir(app));
        install.add_path("/app/node_modules/.bin".to_string());

        // Cypress cache directory
        let all_deps = NodeProvider::get_all_deps(app)?;
        if all_deps.get("cypress").is_some() {
            install.add_cache_directory((*CYPRESS_CACHE_DIR).to_string());
        }

        // Build
        let mut build = Phase::build(NodeProvider::get_build_cmd(app, env)?);

        // Next build cache directories
        let next_cache_dirs = NodeProvider::find_next_packages(app)?;
        for dir in next_cache_dirs {
            let next_cache_dir = ".next/cache";
            build.add_cache_directory(if dir.is_empty() {
                next_cache_dir.to_string()
            } else {
                format!("{}/{}", dir, next_cache_dir)
            });
        }

        // Node modules cache directory
        build.add_cache_directory((*NODE_MODULES_CACHE_DIR).to_string());

        // Start
        let start = NodeProvider::get_start_cmd(app, env)?.map(StartPhase::new);

        let mut plan = BuildPlan::new(&vec![setup, install, build], start);
        plan.add_variables(NodeProvider::get_node_environment_variables());

        Ok(Some(plan))
    }
}

impl NodeProvider {
    pub fn get_node_environment_variables() -> EnvironmentVariables {
        EnvironmentVariables::from([
            ("NODE_ENV".to_string(), "production".to_string()),
            ("NPM_CONFIG_PRODUCTION".to_string(), "false".to_string()),
            ("CI".to_string(), "true".to_string()),
        ])
    }

    pub fn has_script(app: &App, script: &str) -> Result<bool> {
        let package_json: PackageJson = app.read_json("package.json").unwrap_or_default();
        if let Some(scripts) = package_json.scripts {
            if scripts.get(script).is_some() {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub fn get_build_cmd(app: &App, env: &Environment) -> Result<Option<String>> {
        if Nx::is_nx_monorepo(app, env) {
            if let Some(nx_build_cmd) = Nx::get_nx_build_cmd(app, env) {
                return Ok(Some(nx_build_cmd));
            }
        }

        if Turborepo::is_turborepo(app) {
            if let Ok(Some(turbo_build_cmd)) = Turborepo::get_actual_build_cmd(app, env) {
                return Ok(Some(turbo_build_cmd));
            }
        }

        if NodeProvider::has_script(app, "build")? {
            let pkg_manager = NodeProvider::get_package_manager(app);
            Ok(Some(format!("{} run build", pkg_manager)))
        } else {
            Ok(None)
        }
    }

    pub fn get_start_cmd(app: &App, env: &Environment) -> Result<Option<String>> {
        let executor = NodeProvider::get_executor(app);
        let package_json: PackageJson = app.read_json("package.json").unwrap_or_default();

        if Nx::is_nx_monorepo(app, env) {
            if let Some(nx_start_cmd) = Nx::get_nx_start_cmd(app, env)? {
                return Ok(Some(nx_start_cmd));
            }
        }
        if Turborepo::is_turborepo(app) {
            if let Ok(Some(turbo_start_cmd)) =
                Turborepo::get_actual_start_cmd(app, env, &package_json)
            {
                return Ok(Some(turbo_start_cmd));
            }
        }

        let package_manager = NodeProvider::get_package_manager(app);
        if NodeProvider::has_script(app, "start")? {
            return Ok(Some(format!("{} run start", package_manager)));
        }

        if let Some(main) = package_json.main {
            if app.includes_file(&main) {
                return Ok(Some(format!("{} {}", executor, main)));
            }
        }

        if app.includes_file("index.js") {
            return Ok(Some(format!("{} index.js", executor)));
        } else if app.includes_file("index.ts") && package_manager == "bun" {
            return Ok(Some("bun index.ts".to_string()));
        }

        Ok(None)
    }

    /// Parses the package.json engines field and returns a Nix package if available
    pub fn get_nix_node_pkg(
        package_json: &PackageJson,
        app: &App,
        environment: &Environment,
    ) -> Result<Pkg> {
        let env_node_version = environment.get_config_variable("NODE_VERSION");

        let pkg_node_version = package_json
            .engines
            .clone()
            .and_then(|engines| engines.get("node").cloned());

        let nvmrc_node_version = if app.includes_file(".nvmrc") {
            let nvmrc = app.read_file(".nvmrc")?;
            Some(nvmrc.trim().replace('v', ""))
        } else {
            None
        };

        let node_version = env_node_version.or(pkg_node_version).or(nvmrc_node_version);

        let node_version = match node_version {
            Some(node_version) => node_version,
            None => return Ok(Pkg::new(DEFAULT_NODE_PKG_NAME)),
        };

        // Any version will work, use latest
        if node_version == "*" {
            return Ok(Pkg::new(DEFAULT_NODE_PKG_NAME));
        }

        // This also supports 18.x.x, or any number in place of the x.
        let re = Regex::new(r"^(\d*)(?:\.?(?:\d*|[xX]?)?)(?:\.?(?:\d*|[xX]?)?)").unwrap();
        if let Some(node_pkg) = parse_regex_into_pkg(&re, &node_version) {
            return Ok(Pkg::new(node_pkg.as_str()));
        }

        // Parse `>=14.10.3 <16` into nodejs-14_x
        let re = Regex::new(r"^>=(\d+)").unwrap();
        if let Some(node_pkg) = parse_regex_into_pkg(&re, &node_version) {
            return Ok(Pkg::new(node_pkg.as_str()));
        }

        Ok(Pkg::new(DEFAULT_NODE_PKG_NAME))
    }

    pub fn get_package_manager(app: &App) -> String {
        let mut pkg_manager = "npm";
        if app.includes_file("pnpm-lock.yaml") {
            pkg_manager = "pnpm";
        } else if app.includes_file("yarn.lock") {
            pkg_manager = "yarn";
        } else if app.includes_file("bun.lockb") {
            pkg_manager = "bun";
        }
        pkg_manager.to_string()
    }

    pub fn get_package_manager_dlx_command(app: &App) -> String {
        let pkg_manager = NodeProvider::get_package_manager(app);
        match pkg_manager.as_str() {
            "pnpm" => "pnpx",
            "yarn" => "yarn",
            _ => "npx",
        }
        .to_string()
    }

    pub fn get_install_command(app: &App) -> Option<String> {
        if !app.includes_file("package.json") {
            return None;
        }

        let mut install_cmd = "npm i".to_string();
        let package_manager = NodeProvider::get_package_manager(app);
        if package_manager == "pnpm" {
            install_cmd = "pnpm i --frozen-lockfile".to_string();
        } else if package_manager == "yarn" {
            if app.includes_file(".yarnrc.yml") {
                install_cmd = "yarn set version berry && yarn install --check-cache".to_string();
                let yarnrc_yml: Yarnrc = app.read_yaml(".yarnrc.yml").unwrap_or_default();
                if let Some(path) = yarnrc_yml.yarn_path {
                    install_cmd =
                        format!("yarn set version ./{} && yarn install --check-cache", path);
                }
            } else {
                install_cmd = "yarn install --frozen-lockfile".to_string();
            }
        } else if app.includes_file("package-lock.json") {
            install_cmd = "npm ci".to_string();
        } else if app.includes_file("bun.lockb") {
            install_cmd = "bun i --no-save".to_string();
        }

        Some(install_cmd)
    }

    fn get_package_manager_cache_dir(app: &App) -> String {
        let package_manager = NodeProvider::get_package_manager(app);
        if package_manager == "yarn" {
            (*YARN_CACHE_DIR).to_string()
        } else if package_manager == "pnpm" {
            (*PNPM_CACHE_DIR).to_string()
        } else if package_manager == "bun" {
            (*BUN_CACHE_DIR).to_string()
        } else {
            (*NPM_CACHE_DIR).to_string()
        }
    }

    fn get_executor(app: &App) -> String {
        let package_manager = NodeProvider::get_package_manager(app);
        if package_manager == *"bun" {
            "bun"
        } else {
            "node"
        }
        .to_string()
    }

    /// Returns the nodejs nix package and the appropriate package manager nix image.
    pub fn get_nix_packages(app: &App, env: &Environment) -> Result<Vec<Pkg>> {
        let package_json: PackageJson = if app.includes_file("package.json") {
            app.read_json("package.json")?
        } else {
            PackageJson::default()
        };
        let node_pkg = NodeProvider::get_nix_node_pkg(&package_json, app, env)?;

        let pm_pkg: Pkg;
        let mut pkgs = Vec::<Pkg>::new();

        let package_manager = NodeProvider::get_package_manager(app);
        if package_manager != "bun" {
            pkgs.push(node_pkg);
        }
        if package_manager == "pnpm" {
            let lockfile = app.read_file("pnpm-lock.yaml").unwrap_or_default();
            if lockfile.starts_with("lockfileVersion: 5.3") {
                pm_pkg = Pkg::new("pnpm-6_x");
            } else {
                pm_pkg = Pkg::new("pnpm-7_x");
            }
        } else if package_manager == "yarn" {
            pm_pkg = Pkg::new("yarn-1_x");
        } else if package_manager == "bun" {
            pm_pkg = Pkg::new("bun");
        } else {
            // npm
            let lockfile = app.read_file("package-lock.json").unwrap_or_default();
            if lockfile.contains("\"lockfileVersion\": 1") {
                pm_pkg = Pkg::new("npm-6_x");
            } else {
                pm_pkg = Pkg::new("npm-8_x");
            }
        };
        pkgs.push(pm_pkg.from_overlay(NODE_OVERLAY));

        Ok(pkgs)
    }

    pub fn uses_node_dependency(app: &App, dependency: &str) -> bool {
        [
            "package.json",
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
        ]
        .iter()
        .any(|file| app.read_file(file).unwrap_or_default().contains(dependency))
    }

    pub fn find_next_packages(app: &App) -> Result<Vec<String>> {
        // Find all package.json files
        let package_json_files = app.find_files("**/package.json")?;

        let mut cache_dirs: Vec<String> = vec![];

        // Find package.json files with a "next build" build script and cache the associated .next/cache directory
        for file in package_json_files {
            // Don't find package.json files that are in node_modules
            if file
                .as_path()
                .to_str()
                .unwrap_or_default()
                .contains("node_modules")
            {
                continue;
            }

            let json: PackageJson = app.read_json(file.to_str().unwrap())?;
            let deps = NodeProvider::get_deps_from_package_json(&json);
            if deps.contains("next") {
                let relative = app.strip_source_path(file.as_path())?;
                cache_dirs.push(relative.parent().unwrap().to_slash().unwrap().into_owned());
            }
        }

        Ok(cache_dirs)
    }

    /// Finds all dependencies (dev and non-dev) of all package.json files in the app.
    pub fn get_all_deps(app: &App) -> Result<HashSet<String>> {
        // Find all package.json files
        let package_json_files = app.find_files("**/package.json")?;

        let mut all_deps: HashSet<String> = HashSet::new();

        for file in package_json_files {
            if file
                .as_path()
                .to_str()
                .unwrap_or_default()
                .contains("node_modules")
            {
                continue;
            }

            let json: PackageJson = app.read_json(file.to_str().unwrap())?;

            all_deps.extend(NodeProvider::get_deps_from_package_json(&json));
        }

        Ok(all_deps)
    }

    pub fn get_deps_from_package_json(json: &PackageJson) -> HashSet<String> {
        let mut all_deps: HashSet<String> = HashSet::new();

        let deps = json
            .dependencies
            .clone()
            .map(|deps| deps.keys().cloned().collect::<Vec<String>>())
            .unwrap_or_default();

        let dev_deps = json
            .dev_dependencies
            .clone()
            .map(|dev_deps| dev_deps.keys().cloned().collect::<Vec<String>>())
            .unwrap_or_default();

        all_deps.extend(deps.into_iter());
        all_deps.extend(dev_deps.into_iter());

        all_deps
    }
}

fn version_number_to_pkg(version: u32) -> String {
    if AVAILABLE_NODE_VERSIONS.contains(&version) {
        format!("nodejs-{}_x", version)
    } else {
        DEFAULT_NODE_PKG_NAME.to_string()
    }
}

fn parse_regex_into_pkg(re: &Regex, node_version: &str) -> Option<String> {
    let matches: Vec<_> = re.captures_iter(node_version).collect();
    if let Some(captures) = matches.get(0) {
        match captures[1].parse::<u32>() {
            Ok(version) => return Some(version_number_to_pkg(version)),
            Err(_e) => {}
        }
    }

    None
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;

    use super::*;

    fn engines_node(version: &str) -> HashMap<String, String> {
        HashMap::from([("node".to_string(), version.to_string())])
    }

    #[test]
    fn test_no_engines() -> Result<()> {
        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?,
            Pkg::new(DEFAULT_NODE_PKG_NAME)
        );

        Ok(())
    }

    #[test]
    fn test_star_engine() -> Result<()> {
        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    engines: Some(engines_node("*")),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?,
            Pkg::new(DEFAULT_NODE_PKG_NAME)
        );

        Ok(())
    }

    #[test]
    fn test_simple_engine() -> Result<()> {
        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    engines: Some(engines_node("14")),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?,
            Pkg::new("nodejs-14_x")
        );

        Ok(())
    }

    #[test]
    fn test_simple_engine_x() -> Result<()> {
        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    engines: Some(engines_node("18.x")),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?,
            Pkg::new("nodejs-18_x")
        );

        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    engines: Some(engines_node("14.X")),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?,
            Pkg::new("nodejs-14_x")
        );

        Ok(())
    }

    #[test]
    fn test_advanced_engine_x() -> Result<()> {
        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    engines: Some(engines_node("18.x.x")),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?,
            Pkg::new("nodejs-18_x")
        );

        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    engines: Some(engines_node("14.X.x")),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?,
            Pkg::new("nodejs-14_x")
        );

        Ok(())
    }

    #[test]
    fn test_advanced_engine_number() -> Result<()> {
        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    engines: Some(engines_node("18.4.2")),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?,
            Pkg::new("nodejs-18_x")
        );

        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    engines: Some(engines_node("14.8.x")),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?,
            Pkg::new("nodejs-14_x")
        );

        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    engines: Some(engines_node("14.x.8")),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?,
            Pkg::new("nodejs-14_x")
        );

        Ok(())
    }

    #[test]
    fn test_engine_range() -> Result<()> {
        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    engines: Some(engines_node(">=14.10.3 <16")),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?,
            Pkg::new("nodejs-14_x")
        );

        Ok(())
    }

    #[test]
    fn test_version_from_environment_variable() -> Result<()> {
        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::new(BTreeMap::from([(
                    "NIXPACKS_NODE_VERSION".to_string(),
                    "14".to_string()
                )]))
            )?,
            Pkg::new("nodejs-14_x")
        );

        Ok(())
    }

    #[test]
    fn test_version_from_nvmrc() -> Result<()> {
        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    ..Default::default()
                },
                &App::new("examples/node-nvmrc")?,
                &Environment::default()
            )?,
            Pkg::new("nodejs-14_x")
        );

        Ok(())
    }

    #[test]
    fn test_engine_invalid_version() -> Result<()> {
        // this test now defaults to lts
        assert_eq!(
            NodeProvider::get_nix_node_pkg(
                &PackageJson {
                    name: Some(String::default()),
                    engines: Some(engines_node("15")),
                    ..Default::default()
                },
                &App::new("examples/node")?,
                &Environment::default()
            )?
            .name,
            DEFAULT_NODE_PKG_NAME
        );

        Ok(())
    }

    #[test]
    fn test_find_next_packages() -> Result<()> {
        assert_eq!(
            NodeProvider::find_next_packages(&App::new("./examples/node-monorepo")?)?,
            vec!["packages/client".to_string()]
        );
        assert_eq!(
            NodeProvider::find_next_packages(&App::new(
                "./examples/node-monorepo/packages/client"
            )?)?,
            vec![String::new()]
        );

        Ok(())
    }
}
