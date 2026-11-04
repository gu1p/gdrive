use crate::{common::retry::retry_api, hub::Hub};

pub(crate) async fn generate_ids(
    hub: &Hub,
    count: i32,
) -> Result<Vec<String>, Box<google_drive3::Error>> {
    let (_, ids) = retry_api(
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
