
fn handle_keychain_subcommand(data_dir: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");

    match sub {
        "status" => {
            let available = rustykrab_store::keychain::keychain_available();
            println!(
                "OS credential store: {}",
                if available {
                    if cfg!(target_os = "macos") {
                        "available (macOS Data Protection Keychain)"
                    } else {
                        "available (Secret Service)"
                    }
                } else {
                    "not available"
                }
            );

            if !available {
                println!(
                    "\nNo OS credential store detected. Use environment variables \
                     or the encrypted store instead."
                );
                return Ok(());
            }

            // Master key (separate service).
            let master_status = match rustykrab_store::keychain::get_credential(
                "com.rustykrab.master-key",
                "rustykrab-encryption-key",
            ) {
                Ok(Some(_)) => "present",
                Ok(None) => "not set",
                Err(_) => "error",
            };

            let svc = rustykrab_store::registry::keychain_service();
            println!(
                "\n{:<25} {:<25} {:<10} STATUS",
                "CREDENTIAL", "ACCOUNT", "REQUIRED"
            );
            println!("{}", "-".repeat(75));
            println!(
                "{:<25} {:<25} {:<10} {}",
                "Master key", "rustykrab-encryption-key", "-", master_status
            );

            // All registry entries.
            for spec in rustykrab_store::registry::REGISTRY {
                let status =
                    match rustykrab_store::keychain::get_credential(svc, spec.keychain_account) {
                        Ok(Some(_)) => "present",
                        Ok(None) => "not set",
                        Err(_) => "error",
                    };
                let req = if spec.required { "yes" } else { "no" };
                println!(
                    "{:<25} {:<25} {:<10} {}",
                    spec.description, spec.keychain_account, req, status
                );
            }
            println!();
        }

        "set" => {
            // Build the list of accepted names from the registry.
            let known_names: Vec<&str> = rustykrab_store::registry::REGISTRY
                .iter()
                .map(|s| s.keychain_account)
                .collect();
            let names_list = known_names.join(", ");

            let name = args.get(1).ok_or_else(|| {
                anyhow::anyhow!(
                    "usage: rustykrab-cli keychain set <name> <value>\n  \
                     names: {names_list}, or <service>:<account>"
                )
            })?;
            let value = args.get(2).ok_or_else(|| {
                anyhow::anyhow!("usage: rustykrab-cli keychain set <name> <value>")
            })?;

            if !rustykrab_store::keychain::keychain_available() {
                anyhow::bail!("OS credential store is not available on this platform");
            }

            let svc = rustykrab_store::registry::keychain_service();

            // Look up the name in the registry first, then fall back to
            // arbitrary service:account pairs.
            let (service, account, store_name) =
                if let Some(spec) = rustykrab_store::registry::lookup_by_account(name) {
                    (svc, spec.keychain_account, Some(spec.store_name))
                } else if let Some((s, a)) = name.split_once(':') {
                    (s, a, None)
                } else {
                    anyhow::bail!(
                        "unknown credential name '{name}'. \
                         Use: {names_list}, or <service>:<account>"
                    );
                };

            rustykrab_store::keychain::set_credential(service, account, value)
                .map_err(|e| anyhow::anyhow!("failed to store: {e}"))?;
            println!("Stored in credential store: {service}/{account}");

            // Also persist to the encrypted store if the DB exists.
            if let Some(sn) = store_name {
                let db_path = data_dir.join("db");
                if db_path.exists() {
                    if let Ok(master_key) = rustykrab_store::keychain::resolve_master_key() {
                        if let Ok(store) = rustykrab_store::Store::open(&db_path, master_key) {
                            let _ = store.secrets().set(sn, value);
                            println!("Also stored in encrypted store as '{sn}'");
                        }
                    }
                }
            }
        }

        "migrate" => {
            if !rustykrab_store::keychain::keychain_available() {
                anyhow::bail!("OS credential store is not available on this platform");
            }

            println!("Migrating credentials to OS credential store...\n");

            // Re-resolve master key — this ensures it is stored in the credential store.
            match rustykrab_store::keychain::resolve_master_key() {
                Ok(_) => println!("  master key: OK"),
                Err(e) => println!("  master key: FAILED ({e})"),
            }

            // Migrate all registry secrets from the encrypted store.
            let db_path = data_dir.join("db");
            if db_path.exists() {
                if let Ok(master_key) = rustykrab_store::keychain::resolve_master_key() {
                    if let Ok(store) = rustykrab_store::Store::open(&db_path, master_key) {
                        let svc = rustykrab_store::registry::keychain_service();
                        for spec in rustykrab_store::registry::REGISTRY {
                            if let Ok(val) = store.secrets().get(spec.store_name) {
                                match rustykrab_store::keychain::set_credential(
                                    svc,
                                    spec.keychain_account,
                                    &val,
                                ) {
                                    Ok(()) => {
                                        println!("  {}: migrated", spec.description)
                                    }
                                    Err(e) => {
                                        println!("  {}: FAILED ({e})", spec.description)
                                    }
                                }
                            } else {
                                println!(
                                    "  {}: not in store (set via 'keychain set {}')",
                                    spec.description, spec.keychain_account
                                );
                            }
                        }
                    }
                }
            } else {
                println!(
                    "  No database found at {} — skipping store migration",
                    db_path.display()
                );
            }

            println!("\nMigration complete. Restart rustykrab-cli to verify.");
        }

        _ => {
            eprintln!("Unknown keychain subcommand: {sub}");
            eprintln!("Usage:");
            eprintln!("  rustykrab-cli keychain status              Show credential store status");
            eprintln!("  rustykrab-cli keychain set <name> <value>  Store a credential");
            eprintln!(
                "  rustykrab-cli keychain migrate             Migrate store to OS credential store"
            );
            eprintln!();
            eprint!("Credential names: ");
            let names: Vec<&str> = rustykrab_store::registry::REGISTRY
                .iter()
                .map(|s| s.keychain_account)
                .collect();
            eprintln!("{}, or <service>:<account>", names.join(", "));
            std::process::exit(1);
        }
    }

    Ok(())
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}
