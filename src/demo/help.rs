use crate::operator_i18n;

pub fn print_help() {
    println!(
        "{}",
        operator_i18n::tr(
            "demo.repl.help",
            "Available commands:\n  @show              ─ display the last adaptive card summary\n  @json              ─ emit the raw JSON value received from the flow\n  @back              ─ revert to the previous blocked card/inputs\n  @input <k>=<v>     ─ set or override an input field\n  @click <action_id> ─ submit the card with the provided action\n  @setup [provider]  ─ run interactive setup wizard for a provider\n  @help              ─ print this help text\n  @quit              ─ exit the REPL"
        )
    );
}
