# Engine Physics

`engine/engine-physics` содержит CPU-геометрию столкновений и запросы без оконного или GPU-пути. `Shape` поддерживает sphere, box, capsule, convex hull и triangle mesh; формы используют локальные координаты и rigid `Pose` без масштаба. Выпуклые формы предоставляют support mapping, AABB, объём, центр массы и тензор инерции; triangle mesh не имеет динамических mass properties. Quickhull принимает до 4096 входных точек, ограничен 256 вершинами и 256 итерациями. BVH ограничивает mesh одним миллионом вершин и 262144 треугольниками.

`broad::sweep_and_prune` строит детерминированные пары пересекающихся AABB по оси X. Число элементов ограничено 16384, проверок и пар — одним миллионом. `narrow::collide` возвращает контактный manifold до четырёх точек для выпуклых пар и пары convex–triangle mesh; mesh-кандидаты запрашиваются из BVH и проверяются по фактическим треугольникам. Выпуклые запросы используют bounded GJK/EPA; `narrow::distance` возвращает расстояние и мировые witness points. `ManifoldCache` хранит локальные anchors и накопленные импульсы по caller-provided `PairKey`; жизненный цикл пар принадлежит вызывающей симуляции.

GJK ограничен 64 итерациями, EPA — 64 итерациями и 128 гранями. Broadphase и число mesh-кандидатов также ограничены; превышение возвращается как ошибка. `ray::raycast` проверяет sphere, box, capsule, hull planes и BVH triangles в заданной позе. Узкие пары sphere–sphere, sphere–box, sphere–capsule и capsule–capsule используют аналитические контакты; box–box использует SAT. Hull и прочие выпуклые пары проходят support-mapped narrowphase.

`PhysicsWorld` хранит static, kinematic и dynamic тела с одним collider на тело; collider имеет локальный offset, trigger, friction, restitution и enable state. Dynamic-тела интегрируют гравитацию и damping с учётом центра массы и world-space inertia. `solver::solve` выполняет 10 последовательных импульсных итераций, warm start manifold impulses, angular response, restitution, две tangent-направленные Coulomb friction, positional correction и сон при скоростях ниже 0.05 м/с и 0.05 рад/с в течение 0.5 секунды.

Контактные пары создаются, только если хотя бы одно тело dynamic. Trigger проверяется против любого non-trigger collider, включая static и kinematic; trigger–trigger пропускается. Sleep сохраняет активную пару и не создаёт повторный Enter. Команды teleport/disable/remove обновляют пары и будят dynamic-соседей; движущийся kinematic будит контактирующие dynamic-тела. Enter/Exit создаются один раз при изменении набора активных пар.

`StepDriver` применяет команды один раз на каждый полученный `Step`, даже при нуле подшагов. `AddForce` добавляет импульс `force × Step.dt`; отложенная kinematic target сохраняется при нулевом числе подшагов и достигается линейно за выполненные подшаги. Аккумулятор ограничивает число подшагов, лишнее время ограничивается одним fixed step; каждый вызов возвращает poses и events в `StepResult`.

`PhysicsEngine::run` пока остаётся совместимой потоковой заглушкой и ждёт сигнал остановки. `PhysicsWorld` и `StepDriver` — детерминированный CPU API, ещё не подключённый к runtime-потоку, script API или immutable query snapshot.

Источник: `engine/engine-physics/src/{broad.rs,narrow.rs,solver.rs,world.rs,shape.rs,hull.rs,bvh.rs,ray.rs}`.
