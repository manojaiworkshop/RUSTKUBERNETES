import { Suspense } from "react";
import { UsersPage } from "@/features/users/UsersPage";

export default function Page() {
  return <Suspense><UsersPage /></Suspense>;
}
