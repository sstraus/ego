use super::{ScopedSecrets, connections, instance, providers};
use ego_ai::catalogue::{Catalogue, ModelSlug};
use ego_ai::generator::WireGenerator;
use ego_ai::provider::ProviderRow;
use ego_core::budget::BudgetGate;
use ego_core::cost::CurrencyCode;
use ego_core::event::id::GenerationId;
use ego_core::generator::attempt::GenerationAttempt;
use ego_core::generator::port::Generator;
use ego_core::generator::types::{Conversation, GenRequest, Message, Role, ThinkingLevel};
use futures::StreamExt;
use std::sync::Mutex;

// Catches: deployment construction drops the route header before generating the POST.
#[test]
fn vertex_gemini_instance_generation_sends_route_auth_headers() {
    futures::executor::block_on(async {
        let mut row = ProviderRow::vertex_gemini();
        row.extra_headers
            .push(("x-goog-user-project".into(), "quota-project".into()));
        let spec = instance(
            "https://aiplatform.googleapis.com/v1/projects/work-project/locations/global/",
        );
        let catalogue = Catalogue::new([row])
            .with_instances(&providers(spec), &connections())
            .unwrap();
        let slug = ModelSlug::parse("office-google/messages-looking-alias").unwrap();
        let credential = catalogue
            .row("office-google")
            .unwrap()
            .credential
            .resolve(&ScopedSecrets(Mutex::new(Vec::new())))
            .unwrap();
        let (http, recorded) = crate::support::unasked();
        let resolved = catalogue.resolve(&slug, &http, &credential).await.unwrap();
        let request = GenRequest {
            model: resolved.model,
            messages: Conversation::new(vec![Message::text(Role::User, "Summarize the report")])
                .unwrap(),
            tools: Vec::new(),
            thinking: ThinkingLevel::Medium,
            max_output_tokens: Some(2048),
            parallel_tool_calls: false,
            web_search: None,
            cache: None,
        };
        let mut gate = BudgetGate::unlimited(CurrencyCode::new("USD").unwrap());
        let admission = gate
            .admit(&resolved.metering, None, &request, &resolved.capabilities)
            .unwrap();
        let attempt = GenerationAttempt::new(
            GenerationId::new(),
            admission,
            &resolved.metering,
            None,
            &request,
            &resolved.capabilities,
        )
        .unwrap();
        let generator = WireGenerator::new(resolved.row, resolved.wire, http, credential);
        let _: Vec<_> = generator.generate(request, attempt).collect().await;
        let [post] = &recorded.asked()[..] else {
            panic!("expected one Vertex generation request");
        };
        assert!(
            post.headers
                .iter()
                .any(|(name, value)| { name == "x-goog-user-project" && value == "quota-project" }),
            "Vertex generation omitted its configured quota-project header"
        );
    });
}
