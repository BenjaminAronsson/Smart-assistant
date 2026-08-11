// `dashboard/v1` — the host's template, not the model's code (ADR-029).
//
// The spec is DATA: imported as JSON, written by the builder, never spliced into
// this file. Everything below reads it through the DOM's text/attribute APIs —
// there is no `innerHTML`, no `eval`, and no string-built markup — so a hostile
// title or binding target is displayed, never interpreted, even though the host
// has already validated both.
//
// Values arrive later, through the F6.5 capability bridge: a card starts as
// "no data" and is filled only if the host answers a request for a capability
// the app DECLARED and was GRANTED. Declaring is not authorizing (invariant 1),
// so this file must render correctly when every request is refused — which is
// exactly what it does today, before the bridge exists.
import spec from "./spec.json";
import "./style.css";

/** A labelled card, empty until (and unless) the bridge fills it. */
function card(binding) {
  const el = document.createElement("section");
  el.className = "card";
  el.dataset.binding = binding.name;
  el.dataset.capability = binding.capability;

  const label = document.createElement("h2");
  label.textContent = binding.name.replace(/_/g, " ");
  el.append(label);

  const value = document.createElement("p");
  value.className = "value";
  value.textContent = "—";
  el.append(value);

  const target = document.createElement("p");
  target.className = "target";
  target.textContent = binding.target;
  el.append(target);

  return el;
}

function render(root) {
  const header = document.createElement("header");
  const title = document.createElement("h1");
  title.textContent = spec.title;
  header.append(title);
  root.append(header);

  if (spec.bindings.length === 0) {
    const empty = document.createElement("p");
    empty.className = "empty";
    empty.textContent = "This app declares no data.";
    root.append(empty);
    return;
  }

  const grid = document.createElement("div");
  grid.className = "grid";
  for (const binding of spec.bindings) grid.append(card(binding));
  root.append(grid);
}

render(document.getElementById("app"));
