import { Suspense } from "react";
import { ClustersPage } from "@/features/clusters/ClustersPage";

export default function Page() {
  return <Suspense><ClustersPage /></Suspense>;
}
