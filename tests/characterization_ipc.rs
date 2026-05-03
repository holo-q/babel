use babel::ipc::{self, TitleTarget};
use babel::utility::ipc as transport_ipc;
use serde_json::json;

fn accepts_canonical_request(_: ipc::Request) {}
fn accepts_canonical_response(_: ipc::Response) {}
fn accepts_canonical_title_target(_: ipc::TitleTarget) {}

#[test]
fn ipc_dtos_have_canonical_module_and_compat_transport_reexport() {
    accepts_canonical_request(transport_ipc::Request::Ping);
    accepts_canonical_response(transport_ipc::Response::Ok {
        message: "ok".to_string(),
    });
    accepts_canonical_title_target(transport_ipc::TitleTarget::Pane {
        os_window_id: 11,
        pane_id: 7,
    });
}

#[test]
fn request_wire_shape_survives_dto_extraction() -> anyhow::Result<()> {
    let request = ipc::Request::GetTitle {
        target: TitleTarget::Pane {
            os_window_id: 11,
            pane_id: 7,
        },
    };

    assert_eq!(
        serde_json::to_value(request)?,
        json!({
            "cmd": "get_title",
            "target": {
                "kind": "pane",
                "os_window_id": 11,
                "pane_id": 7
            }
        })
    );

    Ok(())
}

#[test]
fn response_wire_shape_survives_dto_extraction() -> anyhow::Result<()> {
    let response = ipc::Response::PendingInput {
        window_id: 7,
        has_pending: true,
        pending_text: Some("draft".to_string()),
    };

    assert_eq!(
        serde_json::to_value(response)?,
        json!({
            "status": "pending_input",
            "window_id": 7,
            "has_pending": true,
            "pending_text": "draft"
        })
    );

    Ok(())
}
