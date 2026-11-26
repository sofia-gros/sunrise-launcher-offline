//! Queueing installs from the window: GitHub, local and registry changes, play after.
use super::*;

impl Launcher {
    pub(crate) fn stop_registry_install(&mut self) {
        let Some(cancellation) = self
            .mods
            .installs
            .running
            .as_ref()
            .map(|run| run.cancellation.clone())
        else {
            return;
        };
        cancellation.stop();
        if cancellation.is_stopped() {
            self.answer_source_question(Ok(false));
            if let Some((_, reply)) = self.mods.installs.world.take() {
                let _ = reply.send(false);
            }
            if let Some((_, reply)) = self.mods.installs.question.take() {
                let _ = reply.send(false);
            }
        }
    }

    pub(crate) fn registry_install_stopping(&self) -> bool {
        self.mods
            .installs
            .running
            .as_ref()
            .is_some_and(|run| run.cancellation.is_stopped())
    }

    pub(crate) fn registry_accepts_play(&self) -> bool {
        self.selected_instance()
            .is_some_and(|instance| self.mods.installs.accepts_play(instance))
    }

    pub(crate) fn defer_registry_play(
        &mut self,
        instance: &instances::Instance,
        intent: launcher_play::PlayIntent,
    ) {
        if self.mods.installs.defer_play(instance, intent) {
            self.open_sheet(crate::sheets::Sheet::RegistryPlay);
        }
    }

    pub(crate) fn github_target(&self) -> Result<GithubTarget, String> {
        if !install_has_player(
            &self.accounts,
            sunrise_launcher::developer_offline::selected(),
        ) {
            return Err(tr("Installs after you sign in").into());
        }
        if let Some(why) = self.mods_locked() {
            return Err(why.into());
        }
        let profile = self
            .selected_instance()
            .cloned()
            .ok_or_else(backend::no_version_installed)?;
        if profile.kind == instances::Kind::Pack {
            return Err(tr("This pack controls which mods it installs.").into());
        }
        let target = self.profile_target(&profile);
        let release = target
            .as_deref()
            .and_then(|tag| self.release_to_install(tag));
        let profile_target = sunrise::Target::of(&profile)?;
        Ok(GithubTarget {
            identity: profile_target.identity(),
            profile_target,
            profile,
            target,
            release,
        })
    }

    pub(crate) fn install_base(&self, profile: &instances::Instance) -> Option<&str> {
        self.mods.installs.base(profile)
    }

    pub(crate) fn queue_github(&mut self, input: Arc<GithubInput>) -> Result<(), String> {
        let target = &input.target;
        if !target.same_destination(&self.github_target()?) {
            return Err(tr(
                "The profile or game build changed. Check the source again; nothing was changed.",
            )
            .into());
        }
        let (kind, ids): (Kind, Vec<String>) = match &input.selection {
            GithubSelection::Add(packages) => (
                Kind::Github,
                packages.iter().map(|p| p.id.clone()).collect(),
            ),
            GithubSelection::Source(packages) => (
                Kind::Source,
                packages.iter().map(|p| p.origin().id.clone()).collect(),
            ),
        };
        if kind == Kind::Source {
            for id in &ids {
                self.mods
                    .installs
                    .source_outcomes
                    .remove(&(target.identity, id.clone()));
            }
        }
        self.mods.installs.submit(Request {
            identity: target.identity,
            profile: target.profile.clone(),
            profile_target: target.profile_target.clone(),
            target: target.target.clone(),
            release: target.release.clone(),
            displayed_names: BTreeMap::new(),
            view: RequestView {
                kind,
                card: input.link.clone(),
                ids,
            },
            github: Some(input),
            local: None,
            sources: Vec::new(),
            picks: None,
        });
        self.start_registry_queue();
        Ok(())
    }

    pub(crate) fn queue_local(&mut self, input: Arc<LocalInput>) -> Result<(), String> {
        let target = &input.target;
        if !target.same_destination(&self.github_target()?) {
            return Err(tr(
                "The profile or game build changed. Check the source again; nothing was changed.",
            )
            .into());
        }
        let ids: Vec<String> = input.packages.iter().map(|p| p.id.clone()).collect();
        self.mods.installs.submit(Request {
            identity: target.identity,
            profile: target.profile.clone(),
            profile_target: target.profile_target.clone(),
            target: target.target.clone(),
            release: target.release.clone(),
            displayed_names: BTreeMap::new(),
            view: RequestView {
                kind: Kind::Local,
                card: input.label.clone(),
                ids,
            },
            github: None,
            local: Some(input),
            sources: Vec::new(),
            picks: None,
        });
        self.start_registry_queue();
        Ok(())
    }

    pub(crate) fn queue_registry_change(
        &mut self,
        kind: Kind,
        card: String,
        ids: Vec<String>,
        displayed_names: BTreeMap<String, String>,
    ) -> Result<(), String> {
        if ids.is_empty() {
            return Ok(());
        }
        if kind == Kind::InstallV2
            && !install_has_player(
                &self.accounts,
                sunrise_launcher::developer_offline::selected(),
            )
        {
            return Err(tr("Installs after you sign in").into());
        }
        if let Some(why) = self.mods_locked() {
            return Err(why.into());
        }
        let instance = self
            .selected_instance()
            .cloned()
            .ok_or_else(backend::no_version_installed)?;
        if instance.kind == instances::Kind::Pack {
            return Err(tr("This pack controls which mods it installs.").into());
        }
        let target = self.profile_target(&instance);
        let release = target
            .as_deref()
            .and_then(|tag| self.release_to_install(tag));
        let profile_target = sunrise::Target::of(&instance)?;
        self.mods.installs.submit(Request {
            identity: profile_target.identity(),
            profile_target,
            profile: instance,
            target,
            release,
            displayed_names,
            view: RequestView { kind, card, ids },
            github: None,
            local: None,
            sources: Vec::new(),
            picks: None,
        });
        self.start_registry_queue();
        Ok(())
    }

    pub(in crate::mods_page) fn start_registry_queue(&mut self) -> bool {
        if self.mods_locked().is_some()
            || self.mutating()
            || self.mods.installs.question.is_some()
            || self.mods.installs.world.is_some()
        {
            return false;
        }
        let Some(request) = self.mods.installs.waiting.pop_front() else {
            return false;
        };
        // Pressing Add again reads the source again: its last refusal goes until this run says.
        let identity = request.identity;
        self.mods
            .installs
            .source_outcomes
            .retain(|(who, id), _| *who != identity || !request.view.ids.contains(id));
        let kind = request.view.kind;
        let ids = request.view.ids.clone();
        let cancellation = Cancellation::default();
        let listed = request
            .picks
            .as_ref()
            .is_none_or(|picks| self.source_picks_listed(picks.listings()));
        let source = (kind == Kind::Source).then(|| RunningSource {
            ids: ids.clone(),
            members: if let Some(picks) = &request.picks {
                picks
                    .listings()
                    .iter()
                    .map(|source| {
                        (
                            source.id().to_owned(),
                            source.name().to_owned(),
                            source.repo().to_owned(),
                        )
                    })
                    .collect()
            } else {
                request
                    .source_inputs()
                    .iter()
                    .map(|source| {
                        let origin = source.origin();
                        (
                            origin.id.clone(),
                            source.name().to_owned(),
                            format!("{}/{}", origin.owner, origin.repo),
                        )
                    })
                    .collect()
            },
            update: request
                .source_inputs()
                .iter()
                .any(PreparedSource::is_update),
            names: if let Some(picks) = &request.picks {
                picks
                    .listings()
                    .iter()
                    .map(|source| source.name().into())
                    .collect()
            } else {
                request
                    .source_inputs()
                    .iter()
                    .map(|source| source.name().into())
                    .collect()
            },
            cancellation: cancellation.clone(),
            rejected: BTreeMap::new(),
        });
        self.mods.installs.running = Some(Running {
            profile: request.profile.clone(),
            identity: request.identity,
            target: request.target.clone(),
            requests: vec![request.view.clone()],
            stage: InstallProgress::Checking,
            eta: crate::eta::StageEta::default(),
            cancellation: cancellation.clone(),
            source,
            github: request.github.clone(),
            local: request.local.clone(),
        });
        let tx = self.mods.tx.clone();
        let launcher = self.tx.clone();
        std::thread::spawn(move || {
            let post = |event| tx.send(Event::Registry(event)).is_ok();
            let choose = || {
                if let Some(tag) = request
                    .target
                    .as_deref()
                    .filter(|tag| backend::installed().iter().any(|local| local == tag))
                {
                    FirstBuild::installed(tag)
                } else if let Some(release) = &request.release {
                    FirstBuild::download(release)
                } else {
                    Err(backend::no_version_installed())
                }
            };
            let hooks = InstallHooks {
                progress: &|step| {
                    let _ = post(Worker::Progress(step));
                },
                continue_after_failure: &|notice| {
                    let (reply, answer) = std::sync::mpsc::channel();
                    post(Worker::Decide(notice.clone(), reply)) && answer.recv().unwrap_or(false)
                },
                cancellation: Some(&cancellation),
            };
            let approve = |impact: &ChangeImpact| {
                if cancellation.is_stopped() {
                    return false;
                }
                let (reply, answer) = std::sync::mpsc::channel();
                let question = WorldQuestion::new(impact, &request.displayed_names);
                post(Worker::World(question, reply)) && answer.recv().unwrap_or(false)
            };
            let authorize = |prepared: &[PreparedSource], dependencies: &[SourceDependency]| {
                let question = SourceQuestion {
                    base: prepared[0].base().into(),
                    mods: prepared
                        .iter()
                        .map(|item| (item.name().into(), item.origin().clone()))
                        .collect(),
                    dependencies: dependencies.to_vec(),
                };
                let (reply, answer) = std::sync::mpsc::channel();
                if !post(Worker::Source(question, reply)) {
                    return Ok(false);
                }
                answer.recv().unwrap_or(Ok(false))
            };
            let confirm_publisher_change = |question: &PublisherChangeQuestion| {
                let (reply, answer) = std::sync::mpsc::channel();
                launcher
                    .send(Msg::PublisherChangeQuestion(
                        Box::new(question.clone()),
                        reply,
                    ))
                    .is_ok()
                    && answer.recv().unwrap_or(false)
            };
            let choose_provider = |need: &ProviderChoiceNeeded| {
                let (reply, answer) = std::sync::mpsc::channel();
                launcher
                    .send(Msg::ProviderChoice(Box::new(need.clone()), reply))
                    .ok()
                    .and_then(|()| answer.recv().ok().flatten())
            };
            let v2_hooks = V2Hooks {
                install: hooks,
                choose_provider: Some(&choose_provider),
                confirm_publisher_change: Some(&confirm_publisher_change),
            };
            let result = match kind {
                Kind::InstallV2 => sunrise::install_registry_v2(
                    &request.profile_target,
                    request.identity,
                    ids.clone(),
                    V2InstallOptions::default(),
                    choose,
                    v2_hooks,
                ),
                Kind::UpdateV2 | Kind::UpdateAllV2 => sunrise::update_registry_v2(
                    &request.profile_target,
                    request.identity,
                    if kind == Kind::UpdateV2 {
                        sunrise::UpdateRequest::Strict(ids)
                    } else {
                        sunrise::UpdateRequest::Bulk(ids)
                    },
                    V2InstallOptions::default(),
                    choose,
                    v2_hooks,
                    &approve,
                ),
                Kind::Github => {
                    let input = request.github.as_ref().expect("a checked GitHub request");
                    match &input.selection {
                        GithubSelection::Add(packages) => sunrise::install_github_manual(
                            &request.profile_target,
                            request.identity,
                            sunrise::ManualRelease {
                                found: &input.source,
                                selected: packages,
                                base: request.target.as_deref(),
                            },
                            choose,
                            hooks,
                            &approve,
                        )
                        .map(|_| InstalledBatch::default()),
                        GithubSelection::Source(_) => {
                            unreachable!("source installs use the source worker")
                        }
                    }
                }
                Kind::Local => {
                    let input = request.local.as_ref().expect("a checked local request");
                    sunrise::install_local(
                        &request.profile_target,
                        request.identity,
                        &input.packages,
                        &cancellation,
                        &|step| {
                            let _ = post(Worker::Progress(step));
                        },
                        &approve,
                    )
                    .map(|_| InstalledBatch::default())
                }
                Kind::Source => {
                    let hooks = SourceHooks {
                        install: hooks,
                        cancellation: &cancellation,
                        authorize: &authorize,
                        approve_worlds: &approve,
                    };
                    let result = if let Some(picks) = &request.picks {
                        (|| {
                            if !listed {
                                return Err(SourceError::Changed(
                                    tr("This source listing changed. Open it again from Discover.")
                                        .into(),
                                ));
                            }
                            let mut rejected = None;
                            let mut pins = Vec::new();
                            let mut resolved = picks.resolve();
                            for listing in picks.listings() {
                                if cancellation.is_stopped() {
                                    return Err(SourceError::Cancelled);
                                }
                                let pin = resolved.next().expect("one resolution per listing");
                                if cancellation.is_stopped() {
                                    return Err(SourceError::Cancelled);
                                }
                                match pin {
                                    Ok(pin) => pins.push(pin),
                                    Err(error) => {
                                        let error = SourceError::Github(error);
                                        let _ = post(Worker::SourceRejected(
                                            listing.id().into(),
                                            error.clone(),
                                        ));
                                        rejected = Some(error);
                                    }
                                }
                            }
                            let added_from = match picks {
                                SourcePicks::One(_) => AddedFrom::Discover,
                                SourcePicks::Collection { id, name, .. } => AddedFrom::Collection {
                                    id: id.clone(),
                                    name: name.clone(),
                                },
                            };
                            let packed = sunrise::pack_sources(&pins, added_from, &cancellation);
                            if cancellation.is_stopped() {
                                return Err(SourceError::Cancelled);
                            }
                            let mut packages = Vec::new();
                            for (pin, result) in pins.iter().zip(packed) {
                                match result {
                                    Ok(package) => packages.push(package),
                                    Err(error) => {
                                        let _ = post(Worker::SourceRejected(
                                            pin.source().id().into(),
                                            error.clone(),
                                        ));
                                        rejected = Some(error);
                                    }
                                }
                            }
                            if cancellation.is_stopped() {
                                return Err(SourceError::Cancelled);
                            }
                            if packages.is_empty() {
                                return Err(rejected.unwrap_or(SourceError::Cancelled));
                            }
                            let result = sunrise::add_sources(
                                &request.profile_target,
                                request.identity,
                                request.target.as_deref().expect("source add has a target"),
                                packages,
                                hooks,
                            );
                            if cancellation.is_stopped() {
                                Err(SourceError::Cancelled)
                            } else {
                                result
                            }
                        })()
                    } else {
                        sunrise::apply_sources(
                            &request.profile_target,
                            request.identity,
                            request.source_inputs(),
                            choose,
                            hooks,
                        )
                    };
                    result
                        .map(|()| InstalledBatch::default())
                        .map_err(InstallError::Source)
                }
                Kind::Remove => sunrise::remove_mods(
                    &request.profile_target,
                    request.identity,
                    &ids,
                    choose,
                    hooks,
                    &approve,
                )
                .map(|_| InstalledBatch::default()),
            };
            let _ = post(Worker::Done(result));
        });
        true
    }
}
