use crate::{common::retry::retry_read, hub::Hub};

pub(crate) async fn generate_ids(
    hub: &Hub,
    count: i32,
) -> Result<Vec<String>, Box<google_drive3::Error>> {
    // A read for the retry policy: generating ids changes nothing on Drive, and ids that a lost
    // reply carried are never used.
    let (_, ids) = retry_read(
        hub.retrier(),
        "files.generateIds",
        |mut delegate| async move {
            hub.files()
                .generate_ids()
                .count(count)
                .add_scope(google_drive3::api::Scope::Full)
                .delegate(&mut delegate)
                .doit()
                .await
        },
    )
    .await?;

    Ok(ids.ids.unwrap_or_default())
}
