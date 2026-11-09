use crate::Resource;
use base64::Engine;
use serde_json::{json, Value};

pub fn resources() -> impl Iterator<Item = Resource> {
    let ns = "production";
    let env: serde_json::Map<String, Value> = [
        ("APP_ENV", "sample"),
        ("DATABASE_URL", "postgres://sample.invalid/payments"),
        ("CACHE_URL", "redis://cache.sample.invalid:6379"),
        ("LOG_LEVEL", "info"),
        ("API_TOKEN", "sample-placeholder"),
        ("S3_BUCKET", "sample-payments"),
        ("S3_PREFIX", "illustrative"),
        ("QUEUE_NAME", "sample-events"),
        ("FEATURE_CHECKOUT", "false"),
        ("SESSION_KEY", "sample-only"),
        ("TELEMETRY_ENDPOINT", "https://telemetry.example.test"),
        ("REGION", "example-region"),
    ]
    .into_iter()
    .map(|(key, value)| {
        (
            key.into(),
            json!(base64::engine::general_purpose::STANDARD.encode(value)),
        )
    })
    .collect();
    let path = |path, service| json!({"path":path,"pathType":"Prefix","backend":{"service":{"name":service,"port":{"number":80}}}});
    let schedule = "0 2 * * *";
    let role_rules =
        json!([{"apiGroups":[""],"resources":["pods","services"],"verbs":["get","list","watch"]}]);
    let account = "observatory-reader";
    let subjects = json!([{"kind":"ServiceAccount","name":account,"namespace":ns}]);
    let fixtures = [
        json!({"kind":"StatefulSet","metadata":{"name":"redis-cache","namespace":ns},"spec":{"serviceName":"redis","replicas":2},"status":{"readyReplicas":2}}),
        json!({"kind":"DaemonSet","metadata":{"name":"node-agent","namespace":"monitoring"},"status":{"desiredNumberScheduled":3,"numberReady":3}}),
        json!({"kind":"Job","metadata":{"name":"payments-reconcile","namespace":ns,"ownerReferences":[{"kind":"CronJob","name":"payments-nightly","controller":true}]},"spec":{"completions":1,"parallelism":1},"status":{"succeeded":1,"conditions":[{"type":"Complete","status":"True"}]}}),
        json!({"kind":"CronJob","metadata":{"name":"payments-nightly","namespace":ns},"spec":{"schedule":schedule,"suspend":false,"concurrencyPolicy":"Forbid"},"status":{"lastScheduleTime":"2026-11-09T02:00:00Z"}}),
        json!({"kind":"CustomResourceDefinition","metadata":{"name":"externalchecks.ops.example.test"},"spec":{"group":"ops.example.test","scope":"Namespaced","names":{"kind":"ExternalCheck","plural":"externalchecks","singular":"externalcheck"},"versions":[{"name":"v1","served":true,"storage":true}]},"status":{"storedVersions":["v1"],"conditions":[{"type":"Established","status":"True"}]}}),
        json!({"kind":"Secret","metadata":{"name":"payments-env","namespace":ns,"creationTimestamp":"2026-11-09T03:30:00Z","labels":{"app":"payments"},"annotations":{"example.test/source":"sample"},"ownerReferences":[{"kind":"ExternalSecret","name":"payments-external","controller":true}]},"type":"Opaque","data":env}),
        json!({"kind":"HorizontalPodAutoscaler","metadata":{"name":"gateway","namespace":ns},"spec":{"minReplicas":2,"maxReplicas":8,"scaleTargetRef":{"kind":"Deployment","name":"gateway"},"metrics":[{"type":"Resource","resource":{"name":"cpu","target":{"type":"Utilization","averageUtilization":70}}}]},"status":{"currentReplicas":2,"desiredReplicas":2}}),
        json!({"kind":"Ingress","metadata":{"name":"public-routes","namespace":ns},"spec":{"ingressClassName":"nginx","rules":[
            {"host":"app.example.test","http":{"paths":[path("/", "web-frontend"),path("/api", "gateway")]}},
            {"host":"orders.example.test","http":{"paths":[path("/", "orders-api")]}},
            {"host":"payments.example.test","http":{"paths":[path("/", "payments")]}}
        ],"tls":[{"hosts":["app.example.test","payments.example.test"],"secretName":"public-routes-tls"}]},"status":{"loadBalancer":{"ingress":[{"hostname":"lb.example.test"},{"ip":"192.0.2.24"}]}}}),
        json!({"kind":"NetworkPolicy","metadata":{"name":"payments-ingress","namespace":ns},"spec":{"podSelector":{"matchLabels":{"app":"payments"}},"policyTypes":["Ingress"],"ingress":[{"from":[{"podSelector":{"matchLabels":{"app":"gateway"}}}],"ports":[{"protocol":"TCP","port":8080}]}]}}),
        json!({"kind":"PersistentVolume","metadata":{"name":"redis-volume"},"spec":{"capacity":{"storage":"10Gi"},"accessModes":["ReadWriteOnce"],"storageClassName":"local-ssd","persistentVolumeReclaimPolicy":"Retain","claimRef":{"name":"redis-data","namespace":ns}},"status":{"phase":"Bound"}}),
        json!({"kind":"StorageClass","metadata":{"name":"local-ssd","annotations":{"storageclass.kubernetes.io/is-default-class":"true"}},"provisioner":"example.test/local","volumeBindingMode":"WaitForFirstConsumer","reclaimPolicy":"Retain"}),
        json!({"kind":"ServiceAccount","metadata":{"name":account,"namespace":ns},"automountServiceAccountToken":false}),
        json!({"kind":"Role","metadata":{"name":"workload-reader","namespace":ns},"rules":role_rules}),
        json!({"kind":"RoleBinding","metadata":{"name":"workload-reader","namespace":ns},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":"workload-reader"},"subjects":subjects}),
        json!({"kind":"ClusterRole","metadata":{"name":"namespace-reader"},"rules":[{"apiGroups":[""],"resources":["namespaces"],"verbs":["get","list"]}]}),
        json!({"kind":"ClusterRoleBinding","metadata":{"name":"namespace-reader"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"namespace-reader"},"subjects":subjects}),
    ];
    fixtures.into_iter().map(Resource::parse)
}
