use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context};
use dialoguer::{Confirm, Editor, Input, Password};
use indicatif::ProgressIterator;
use matrix_sdk::{
    config::SyncSettings,
    crypto::SasState,
    encryption::{
        backups::BackupState,
        identities::UserIdentity,
        recovery::{self, Recovery, RecoveryState},
        verification::VerificationRequestState,
        Encryption,
    },
    ruma::{
        events::{
            key::verification::VerificationMethod,
            room::{
                member::{MembershipState, SyncRoomMemberEvent},
                message::RoomMessageEventContent,
            },
            Mentions, SyncStateEvent,
        },
        RoomId,
    },
    Client, Error, OwnedServerName, Room, RoomState,
};
use rand::distributions::{Alphanumeric, DistString};
use tempfile::tempdir;
use tokio::{
    select,
    signal::ctrl_c,
    time::{self, timeout},
};
use tokio_stream::StreamExt;

const DEVICE_NAME: &str = "migrate-matrix";
const SYNC_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let old_account = login("old")
        .await
        .with_context(|| "login into old account")?;

    let result = first_logined_in(&old_account).await;

    let logout = old_account.matrix_auth().logout().await;

    result?;
    logout?;

    Ok(())
}

async fn first_logined_in(old_account: &Client) -> anyhow::Result<()> {
    let new_account = login("new")
        .await
        .with_context(|| "login into new account")?;

    let result = select! {
        result = second_logined_in(&old_account, &new_account) => result,
        Ok(_) = ctrl_c() => Ok(()),
    };

    let logout = new_account.matrix_auth().logout().await;

    result?;
    logout?;

    Ok(())
}

async fn second_logined_in(old_account: &Client, new_account: &Client) -> anyhow::Result<()> {
    let confirmation = Confirm::new()
        .with_prompt(format!(
            "Do you really want to migrate from {} to {}?",
            old_account
                .user_id()
                .ok_or_else(|| anyhow!("missing user id of old account"))?,
            new_account
                .user_id()
                .ok_or_else(|| anyhow!("missing user id of new account"))?
        ))
        .interact()?;

    if !confirmation {
        println!("Migration cancelled");
        return Ok(());
    }

    let convert_to_dm_msg = Editor::new()
        .edit("Hi, I just changed my Matrix ID. To prevent this chat from breaking, please enter \"/converttodm\" in this chat. Unfortunately, this does not work on every client, but for example Element Desktop/Web does.")?
        .ok_or_else(|| anyhow!("message not confirmed"))?;

    let result_main = account_main(&old_account, &new_account, &convert_to_dm_msg).await;

    let result_backup_new = new_account
        .encryption()
        .backups()
        .wait_for_steady_state()
        .await;

    let result_backup_old = old_account
        .encryption()
        .backups()
        .wait_for_steady_state()
        .await;

    result_main?;
    result_backup_new?;
    result_backup_old?;

    Ok(())
}

async fn account_main(
    old_account: &Client,
    new_account: &Client,
    convert_to_dm_msg: &str,
) -> anyhow::Result<()> {
    let mut rooms = old_account.joined_rooms();

    loop {
        let prev = rooms.len();
        if prev == 0 {
            // no more rooms
            break Ok(());
        }
        println!("Remaining rooms: {prev}");

        let mut errors = vec![];
        for room in rooms.into_iter().progress() {
            let room_id = room.room_id();
            if let Err(e) = migrate_room(old_account, new_account, &room, convert_to_dm_msg).await {
                println!("Error joining room {room_id}: {e:?}");
                errors.push((e, room));
            }
        }

        if errors.len() == prev {
            // no more progress
            let (err, _) = errors.pop().expect("!= 0");
            break Err(err);
        }

        rooms = errors.into_iter().map(|(_, r)| r).collect();
    }
}

async fn migrate_room(
    old_account: &Client,
    new_account: &Client,
    old_room: &Room,
    convert_to_dm_msg: &str,
) -> anyhow::Result<()> {
    let room_id = old_room.room_id();
    let direct = old_room.is_direct().await?;

    println!("Migrating room {room_id} (direct: {direct})");

    let new_room = ensure_joined(old_account, new_account, old_room)
        .await
        .context("ensure joined")?;
    ensure!(new_room.state() == RoomState::Joined);

    let old_user = old_account
        .user_id()
        .ok_or_else(|| anyhow!("missing user id"))?;
    let new_user = new_account
        .user_id()
        .ok_or_else(|| anyhow!("missing user id"))?;

    let old_power_level = old_room.power_levels().await?.for_user(old_user);
    let default_power_level = old_room.power_levels().await?.users_default;
    let leave = if old_power_level != default_power_level {
        old_room
            .update_power_levels(vec![(new_user, old_power_level)])
            .await
            .is_ok()
    } else {
        true
    };

    migrate_keys(old_account.encryption(), new_account.encryption(), room_id).await?;

    if leave {
        old_room.leave().await?;
    }

    new_room.set_is_direct(direct).await?;
    if direct {
        new_room
            .send(
                RoomMessageEventContent::text_plain(convert_to_dm_msg)
                    .add_mentions(Mentions::new()),
            )
            .await?;
    }

    Ok(())
}

async fn migrate_keys(
    old_encryption: Encryption,
    new_encryption: Encryption,
    room_id: &RoomId,
) -> anyhow::Result<()> {
    let passphrase = Alphanumeric.sample_string(&mut rand::thread_rng(), 128);

    let dir = tempdir()?;
    let path = dir.path().join("keys");

    old_encryption
        .backups()
        .download_room_keys_for_room(room_id)
        .await?;

    old_encryption
        .export_room_keys(path.clone(), &passphrase, |s| s.room_id() == room_id)
        .await?;

    new_encryption.import_room_keys(path, &passphrase).await?;

    Ok(())
}

async fn ensure_joined(
    old_account: &Client,
    new_account: &Client,
    old_room: &Room,
) -> anyhow::Result<Room> {
    let room_id = old_room.room_id();

    if let Some(new_room) = new_account.get_room(room_id) {
        match new_room.state() {
            RoomState::Joined => return Ok(new_room),
            RoomState::Invited => {
                new_room.join().await.context("direct invite join failed")?;
                return Ok(new_room);
            }
            _ => {}
        }
    }

    let old_server = old_account
        .user_id()
        .ok_or_else(|| anyhow!("user id missing"))?
        .server_name()
        .to_owned();

    if let Ok(room) = new_account
        .join_room_by_id_or_alias(room_id.into(), &[old_server])
        .await
    {
        return Ok(room);
    }

    let new_user = new_account
        .user_id()
        .ok_or_else(|| anyhow!("missing user id"))?;

    let observer = new_account.observe_room_events::<SyncRoomMemberEvent, Room>(room_id);

    let mut subscriber = observer
        .subscribe()
        .filter(|(e, _)| e.state_key() == new_user); // only listen for own invite

    let (event, new_room) = select! {
       err = old_room.invite_user_by_id(new_user) => {
            err.context("try invite since join failed")?;
            timeout(Duration::from_secs(30), subscriber.next())
                .await
                .map_err(|_| anyhow!("did not get invite within 30 seconds"))?
                .ok_or_else(|| anyhow!("missing event"))?
        },
        event = subscriber.next() => {
            event.ok_or_else(|| anyhow!("missing event"))?
        }
    };

    let old_user = old_account
        .user_id()
        .ok_or_else(|| anyhow!("missing user id"))?;

    ensure!(event.sender() == old_user);
    ensure!(event.membership() == &MembershipState::Invite);

    new_room.join().await.context("try join after new invite")?;
    Ok(new_room)
}

async fn login(name: &str) -> anyhow::Result<Client> {
    let server = Input::<OwnedServerName>::new()
        .with_prompt(format!("{name} server name"))
        .interact()?;

    let client = Client::builder().server_name(&server).build().await?;

    let auth = client.matrix_auth();

    let types = auth.get_login_types().await?;

    if types.flows.iter().any(|f| f.login_type() == "m.login.sso") {
        println!("SSO login supported, starting SSO login for {server}");
        auth.login_sso(|sso_url| async move {
            println!("Open this URL in your browser: {sso_url}");
            Ok(())
        })
        .initial_device_display_name(DEVICE_NAME)
        .await?;
    } else if types
        .flows
        .iter()
        .any(|f| f.login_type() == "m.login.password")
    {
        let username = Input::<String>::new()
            .with_prompt(format!("{name} username"))
            .interact()?;
        let password = Password::new()
            .with_prompt(format!("{name} password"))
            .interact()?;

        println!("Loging in {username} to {server}");
        auth.login_username(username, &password)
            .initial_device_display_name(DEVICE_NAME)
            .await?;
    } else {
        bail!("no supported login type found");
    }

    let result = login_inner(&client).await;

    match result {
        Ok(()) => Ok(client),
        Err(e) => {
            // try cleanup, ignore if it fails
            let _ = auth.logout().await;
            Err(e)
        }
    }
}

async fn login_inner(client: &Client) -> anyhow::Result<()> {
    let user_id = client.user_id().ok_or_else(|| anyhow!("missing user id"))?;

    println!("Login done, syncing user {user_id}");
    client
        .sync_once(SyncSettings::default().timeout(SYNC_TIMEOUT))
        .await?;

    println!("initial sync done for {user_id}");

    {
        let client = client.clone();
        let user_id = user_id.to_owned();
        tokio::spawn(async move {
            loop {
                match client
                    .sync(SyncSettings::default().timeout(SYNC_TIMEOUT))
                    .await
                {
                    Ok(_) => break,
                    Err(Error::Http(e)) => {
                        println!("HTTP error during sync for {user_id}: {e:?}. Waiting 5 seconds.");
                        time::sleep(Duration::from_secs(5)).await;
                    }
                    Err(e) => {
                        println!("Error during sync for {user_id}: {e:?}. Stopping sync.");
                        break;
                    }
                }
            }
        });
    }

    let encryption = client.encryption();

    let identity = encryption
        .get_user_identity(user_id)
        .await?
        .ok_or_else(|| anyhow!("missing own identity"))?;

    do_verification(&identity).await?;

    /*
        let recovery = encryption.recovery();
        let recovery_key = Input::<String>::new()
            .with_prompt(format!("Recovery key"))
            .interact()?;

        recovery.recover(recovery_key.trim()).await?;

        match recovery.state() {
            RecoveryState::Enabled => println!("Successfully recovered all the E2EE secrets."),
            RecoveryState::Disabled => println!("Error recovering, recovery is disabled."),
            RecoveryState::Incomplete => println!("Couldn't recover all E2EE secrets."),
            _ => bail!("We should know our recovery state by now"),
        }

        let state = encryption
            .cross_signing_status()
            .await
            .ok_or_else(|| anyhow!("missing status"))?;
        ensure!(state.is_complete());

        let backups = encryption.backups();
        backups.wait_for_steady_state().await?;
        ensure!(backups.are_enabled().await);
        ensure!(backups.state() == BackupState::Enabled);
    */
    println!("Login done for {user_id}");

    Ok(())
}

async fn do_verification(identity: &UserIdentity) -> anyhow::Result<()> {
    let verification = identity
        .request_verification_with_methods(vec![VerificationMethod::SasV1])
        .await?;

    println!("Waiting for accept. Don't start emoji flow on the other client!");

    let change = verification
        .changes()
        .next()
        .await
        .ok_or_else(|| anyhow!("missing state change"))?;

    let their_methods = match change {
        VerificationRequestState::Ready { their_methods, .. } => their_methods,
        state => bail!("wrong state: {:?}", state),
    };

    ensure!(
        their_methods.contains(&VerificationMethod::SasV1),
        "sas verification not supported"
    );

    let sas = verification
        .start_sas()
        .await?
        .ok_or_else(|| anyhow!("sas start failed"))?;

    let mut stream = sas.changes();
    let emoji = loop {
        return Err(
            match stream
                .next()
                .await
                .ok_or_else(|| anyhow!("missing state change"))?
            {
                SasState::Created { .. } | SasState::Started { .. } | SasState::Accepted { .. } => {
                    continue
                }
                SasState::KeysExchanged { emojis, .. } => break emojis,
                SasState::Cancelled(_) => anyhow!("verification cancelled"),
                s => {
                    anyhow!("unexpected state: {s:?}")
                }
            },
        );
    }
    .ok_or_else(|| anyhow!("emojis missing"))?;

    println!("Emojis: {}", emoji.emojis.map(|e| e.description).join(", "));

    let confirmation = Confirm::new().with_prompt("Do they match?").interact()?;
    if confirmation {
        sas.confirm().await?;
    } else {
        sas.mismatch().await?;
        return Err(anyhow!("emoji missmatch"));
    }

    loop {
        return Err(
            match stream
                .next()
                .await
                .ok_or_else(|| anyhow!("missing state change"))?
            {
                SasState::Confirmed => continue,
                SasState::Done { .. } => break,
                SasState::Cancelled(_) => anyhow!("verification cancelled"),
                s => {
                    anyhow!("unexpected state: {s:?}")
                }
            },
        );
    }
    ensure!(verification.is_done());

    Ok(())
}

async fn wait_for_recovery(recovery: &Recovery) -> anyhow::Result<()> {
    let mut stream = recovery.state_stream();
    loop {
        match stream
            .next()
            .await
            .ok_or_else(|| anyhow!("missing state change"))?
        {
            RecoveryState::Unknown | RecoveryState::Incomplete => continue,
            recovery::RecoveryState::Enabled => break,
            recovery::RecoveryState::Disabled => return Err(anyhow!("recovery disabled")),
        }
    }
    ensure!(recovery.state() == RecoveryState::Enabled);

    Ok(())
}
