import { Suspense } from "react";
import { MachinesPage } from "@/features/machines/MachinesPage";

export default function Page() {
  return <Suspense><MachinesPage /></Suspense>;
}
