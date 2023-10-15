use std::{env::current_dir, time::Duration};

use crate::command::MyCommand;
use crate::host::Config;
use anyhow::{anyhow, Result};
use clap::Parser;
use itertools::Itertools;
use tokio::{task::LocalSet, time::sleep};
use tracing::{error, field, info, info_span, warn, Instrument};

#[derive(Parser, Clone)]
pub struct BuildSystems {
	/// Do not continue on error
	#[clap(long)]
	fail_fast: bool,
	/// Disable automatic rollback
	#[clap(long)]
	disable_rollback: bool,
	/// Run builds as sudo
	#[clap(long)]
	privileged_build: bool,
	#[clap(subcommand)]
	subcommand: Subcommand,
}

enum UploadAction {
	Test,
	Boot,
	Switch,
}
impl UploadAction {
	fn name(&self) -> &'static str {
		match self {
			UploadAction::Test => "test",
			UploadAction::Boot => "boot",
			UploadAction::Switch => "switch",
		}
	}

	pub(crate) fn should_switch_profile(&self) -> bool {
		matches!(self, Self::Switch | Self::Boot)
	}
	pub(crate) fn should_activate(&self) -> bool {
		matches!(self, Self::Switch | Self::Test)
	}
	pub(crate) fn should_schedule_rollback_run(&self) -> bool {
		matches!(self, Self::Switch | Self::Test)
	}
}

enum PackageAction {
	SdImage,
	InstallationCd,
}
impl PackageAction {
	fn build_attr(&self) -> String {
		match self {
			PackageAction::SdImage => "sdImage".to_owned(),
			PackageAction::InstallationCd => "installationCd".to_owned(),
		}
	}
}

enum Action {
	Upload { action: Option<UploadAction> },
	Package(PackageAction),
}
impl Action {
	fn build_attr(&self) -> String {
		match self {
			Action::Upload { .. } => "toplevel".to_owned(),
			Action::Package(p) => p.build_attr(),
		}
	}
}

impl From<Subcommand> for Action {
	fn from(s: Subcommand) -> Self {
		match s {
			Subcommand::Upload => Self::Upload { action: None },
			Subcommand::Test => Self::Upload {
				action: Some(UploadAction::Test),
			},
			Subcommand::Boot => Self::Upload {
				action: Some(UploadAction::Boot),
			},
			Subcommand::Switch => Self::Upload {
				action: Some(UploadAction::Switch),
			},
			Subcommand::SdImage => Self::Package(PackageAction::SdImage),
			Subcommand::InstallationCd => Self::Package(PackageAction::InstallationCd),
		}
	}
}

#[derive(Parser, Clone)]
enum Subcommand {
	/// Upload, but do not switch
	Upload,
	/// Upload + switch to built system until reboot
	Test,
	/// Upload + switch to built system after reboot
	Boot,
	/// Upload + test + boot
	Switch,

	/// Build SD .img image
	SdImage,
	/// Build an installation cd ISO image
	InstallationCd,
}

struct Generation {
	id: u32,
	current: bool,
	datetime: String,
}
async fn get_current_generation(config: &Config, host: &str) -> Result<Generation> {
	let mut cmd = MyCommand::new("nix-env");
	cmd.comparg("--profile", "/nix/var/nix/profiles/system")
		.arg("--list-generations");
	// Sudo is required due to --list-generations acquiring lock on the profile.
	let data = config.run_string_on(&host, cmd, true).await?;
	let generations = data
		.split('\n')
		.map(|e| e.trim())
		.filter(|&l| l != "")
		.filter_map(|g| {
			let gen: Option<Generation> = try {
				let mut parts = g.split_whitespace();
				let id = parts.next()?;
				let id: u32 = id.parse().ok()?;
				let date = parts.next()?;
				let time = parts.next()?;
				let current = if let Some(current) = parts.next() {
					if current == "(current)" {
						Some(true)
					} else {
						None
					}
				} else {
					Some(false)
				};
				let current = current?;
				if parts.next().is_some() {
					warn!("unexpected text after generation: {g}");
				}
				Generation {
					id,
					current,
					datetime: format!("{date} {time}"),
				}
			};
			if gen.is_none() {
				warn!("bad generation: {g}")
			}
			gen
		})
		.collect::<Vec<_>>();
	let current = generations
		.into_iter()
		.filter(|g| g.current)
		.at_most_one()
		.map_err(|_e| anyhow!("bad list-generations output"))?
		.ok_or_else(|| anyhow!("failed to find generation"))?;
	Ok(current)
}

impl BuildSystems {
	async fn build_task(self, config: Config, host: String) -> Result<()> {
		info!("building");
		let action = Action::from(self.subcommand.clone());
		let built = {
			let dir = tempfile::tempdir()?;
			dir.path().to_owned()
		};

		let mut nix_build = MyCommand::new("nix");
		nix_build
			.args([
				"build",
				"--impure",
				"--json",
				// "--show-trace",
				"--no-link",
				"--option",
				"log-lines",
				"200",
			])
			.comparg("--out-link", &built)
			.arg(
				config.configuration_attr_name(&format!(
					"buildSystems.{}.{host}",
					action.build_attr()
				)),
			)
			.args(&config.nix_args);

		if self.privileged_build {
			nix_build = nix_build.sudo();
		}

		nix_build.run_nix().await.map_err(|e| {
			if action.build_attr() == "sdImage" {
				info!("sd-image build failed");
				info!("Make sure you have imported modulesPath/installer/sd-card/sd-image-<arch>[-installer].nix (For installer, you may want to check config)");
				info!("This module was automatically imported before, but was removed for better customization")
			}
			e
		})?;
		let built = std::fs::canonicalize(built)?;

		match action {
			Action::Upload { action } => {
				if !config.is_local(&host) {
					info!("uploading system closure");
					let mut tries = 0;
					loop {
						let mut nix = MyCommand::new("nix");
						nix.arg("copy")
							.arg("--substitute-on-destination")
							.comparg("--to", format!("ssh://root@{host}"))
							.arg(&built);
						match nix.run_nix().await {
							Ok(()) => break,
							Err(e) if tries < 3 => {
								tries += 1;
								warn!("Copy failure ({}/3): {}", tries, e);
								sleep(Duration::from_millis(5000)).await;
							}
							Err(e) => return Err(e),
						}
					}
				}
				if let Some(action) = action {
					let mut failed = false;
					// TODO: Lockfile, to prevent concurrent system switch?
					// TODO: If rollback target exists - bail, it should be removed. Lockfile will not work in case if rollback
					// is scheduler on next boot (default behavior). On current boot - rollback activator will fail due to
					// unit name conflict in systemd-run
					if !self.disable_rollback {
						let _span = info_span!("preparing").entered();
						info!("preparing for rollback");
						let generation = get_current_generation(&config, &host).await?;
						info!(
							"rollback target would be {} {}",
							generation.id, generation.datetime
						);
						{
							let mut cmd = MyCommand::new("sh");
							cmd.arg("-c").arg(format!("mark=$(mktemp -p /etc -t fleet_rollback_marker.XXXXX) && echo -n {} > $mark && mv --no-clobber $mark /etc/fleet_rollback_marker", generation.id));
							if let Err(e) = config.run_on(&host, cmd, true).await {
								error!("failed to set rollback marker: {e}");
								failed = true;
							}
						}
						// Activation script also starts rollback-watchdog.timer, however, it is possible that it won't be started.
						// Kicking it on manually will work best.
						//
						// There wouldn't be conflict, because here we trigger start of the primary service, and systemd will
						// only allow one instance of it.
						if action.should_schedule_rollback_run() {
							let mut cmd = MyCommand::new("systemd-run");
							cmd.comparg("--on-active", "3min")
								.comparg("--unit", "rollback-watchdog-run")
								.arg("systemctl")
								.arg("start")
								.arg("rollback-watchdog.service");
							if let Err(e) = config.run_on(&host, cmd, true).await {
								error!("failed to schedule rollback run: {e}");
								failed = true;
							}
						}
					}
					if action.should_switch_profile() && !failed {
						info!("switching generation");
						let mut cmd = MyCommand::new("nix-env");
						cmd.comparg("--profile", "/nix/var/nix/profiles/system")
							.comparg("--set", &built);
						if let Err(e) = config.run_on(&host, cmd, true).await {
							error!("failed to switch generation: {e}");
							failed = true;
						}
					}
					if action.should_activate() && !failed {
						let _span = info_span!("activating").entered();
						info!("executing activation script");
						let mut switch_script = built.clone();
						switch_script.push("bin");
						switch_script.push("switch-to-configuration");
						let mut cmd = MyCommand::new(switch_script);
						cmd.arg(action.name());
						if let Err(e) = config.run_on(&host, cmd, true).in_current_span().await {
							error!("failed to activate: {e}");
							failed = true;
						}
					}
					if !self.disable_rollback {
						{
							let _span = info_span!("rollback").entered();
							if failed {
								info!("executing rollback");
								let mut cmd = MyCommand::new("systemctl");
								cmd.arg("start").arg("rollback-watchdog.service");
								if let Err(e) = config.run_on(&host, cmd, true).await {
									error!("failed to rollback: {e}");
								}
							} else {
								info!("marking upgrade as successful");
								let mut cmd = MyCommand::new("rm");
								cmd.arg("-f").arg("/etc/fleet_rollback_marker");
								if let Err(e) =
									config.run_on(&host, cmd, true).in_current_span().await
								{
									error!("failed to remove rollback marker. This is bad, as the system will be rolled back by watchdog: {e}")
								}
							}
						}
						{
							let _span = info_span!("disarm").entered();
							info!("disarming watchdog, just in case");
							{
								let mut cmd = MyCommand::new("systemctl");
								cmd.arg("stop").arg("rollback-watchdog.timer");
								if let Err(_e) = config.run_on(&host, cmd, true).await {
									// It is ok, if there was no reboot.
								}
							}
							if action.should_schedule_rollback_run() {
								let mut cmd = MyCommand::new("systemctl");
								cmd.arg("stop").arg("rollback-watchdog-run.timer");
								if let Err(e) = config.run_on(&host, cmd, true).await {
									error!("failed to disarm rollback run: {e}");
								}
							}
						}
					}
				}
			}
			Action::Package(PackageAction::SdImage) => {
				let mut out = current_dir()?;
				out.push(format!("sd-image-{}", host));

				info!("building sd image to {:?}", out);
				let mut nix_build = MyCommand::new("nix");
				nix_build
					.args(["build", "--impure", "--no-link"])
					.comparg("--out-link", &out)
					.arg(config.configuration_attr_name(&format!("buildSystems.sdImage.{}", host,)))
					.args(&config.nix_args);
				if !self.fail_fast {
					nix_build.arg("--keep-going");
				}
				if self.privileged_build {
					nix_build = nix_build.sudo();
				}

				nix_build.run_nix().await?;
			}
			Action::Package(PackageAction::InstallationCd) => {
				let mut out = current_dir()?;
				out.push(format!("installation-cd-{}", host));

				info!("building sd image to {:?}", out);
				let mut nix_build = MyCommand::new("nix");
				nix_build
					.args(["build", "--impure", "--no-link"])
					.comparg("--out-link", &out)
					.arg(
						config.configuration_attr_name(&format!(
							"buildSystems.installationCd.{}",
							host,
						)),
					)
					.args(&config.nix_args);
				if !self.fail_fast {
					nix_build.arg("--keep-going");
				}
				if self.privileged_build {
					nix_build = nix_build.sudo();
				}

				nix_build.run_nix().await?;
			}
		};
		Ok(())
	}

	pub async fn run(self, config: &Config) -> Result<()> {
		let hosts = config.list_hosts().await?;
		let set = LocalSet::new();
		let this = &self;
		for host in hosts.iter() {
			if config.should_skip(host) {
				continue;
			}
			let config = config.clone();
			let host = host.clone();
			let this = this.clone();
			let span = info_span!("deployment", host = field::display(&host));
			set.spawn_local(
				(async move {
					match this.build_task(config, host).await {
						Ok(_) => {}
						Err(e) => {
							error!("failed to deploy host: {}", e)
						}
					}
				})
				.instrument(span),
			);
		}
		set.await;
		Ok(())
	}
}
