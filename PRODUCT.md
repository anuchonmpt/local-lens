# Product

<!-- impeccable:product-schema 1 -->

## Users

DevOps and SRE teams that operate multiple Kubernetes clusters, and application developers who debug workloads on Kubernetes, are equally important. Operators need to move between clusters, triage warnings, and inspect resource health. Developers need to trace a problem from a workload to its Pods, events, logs, and configuration.

## Product Purpose

Cluster Observatory is a local desktop workspace for inspecting and operating Kubernetes clusters. Users should be able to connect with existing credentials, identify the affected cluster and namespace, and move from a health signal to the resource they need to inspect or act on.

## Positioning

Keep the core workflows familiar to Lens users while aiming for low CPU and RAM use, particularly with a cluster Overview left open. Measure app resource use separately from cluster utilization. Use the same cluster and screen when comparing idle performance with Lens.

## Operating Context

- The distributed app is a native macOS desktop app built with Rust and egui. The macOS disk image includes kubectl and supports Apple Silicon and Intel. Kubeconfigs may still depend on external credential helpers.
- Discover existing local kubeconfig contexts and let users switch clusters from the sidebar. Settings supports adding a kubeconfig file, pasting YAML, and syncing a folder.
- Cluster Overview and Nodes are cluster-wide. Namespaced resource views ask users to choose a namespace or explicitly choose **All namespaces**.
- Operators may leave the app open while working elsewhere. The footer exposes the app process's CPU and RAM use, the installed version, and update availability. The app checks for updates at startup and hourly while open, and users choose when to install.

## Capabilities and Constraints

- Browse workloads, configuration, networking, storage, access control, and events. Resource tables support sorting and resizing, and remember widths per table.
- Open resource details in a right-side drawer. Links from Events and Overview Warnings take users to the associated resource. Related views expose configuration and operational actions in context.
- Inspect Pod logs, open a container shell, forward ports, and edit YAML. Kubernetes permissions govern which data and actions are available. Resource mutations use explicit user actions.
- Keep Secret-backed environment variables individually revealable. Secret details display Base64 per key initially, and users can reveal decoded text one key at a time.
- Show CPU and memory utilization when the cluster exposes metrics. Overview history accumulates while connected, and resource details can use Prometheus history when available. The topology view maps Kubernetes relationships and displays measured traffic when the required telemetry is available.

## Brand Commitments

Use **Cluster Observatory** as the product name. Lens is the reference for core Kubernetes workflows and terminology. The app has its own icon in `assets/app-icon.png`.

## Evidence on Hand

- `README.md` documents installation, connection, and cluster workflows. It records an isolated Kind smoke test covering resource inspection and common operational actions.
- `CHANGELOG.md` and `src/` describe shipped behavior. Use the source when older setup copy and current behavior differ.

## Product Principles

- Prioritize the shared workflow from cluster selection to resource details. Make the next step legible to operators and developers alike.
- Keep cluster and namespace scope clear wherever users inspect data or act on resources.
- Treat idle CPU and RAM as product quality. Check them when changing refresh, metrics collection, or background work.
- Show what the cluster actually reports and distinguish unavailable data from zero usage. Reveal sensitive values and change cluster resources through deliberate user actions.
